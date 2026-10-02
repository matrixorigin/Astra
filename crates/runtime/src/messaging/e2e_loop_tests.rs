//! End-to-end tests for messaging integration with the agentic loop.
//!
//! Verifies:
//! 1. The legacy standalone send_message schema is never injected
//! 2. Queued messages reach volatile model context without application receipts
//! 3. Execution progress stays on the live projection instead of the mailbox

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{Map, Value, json};

    use crate::messaging::reply_obligations::ReplyObligations;
    use crate::orchestration::permission_sync::{
        InheritedPermissions, PermissionMode, PermissionRequest, PermissionRequestMessaging,
        PermissionResponse, PermissionResponseMessaging, PermissionSyncContext,
    };
    use crate::orchestration::spawner::FanoutParentAdmission;
    use crate::server::delegation::engine::{DelegationTracker, SubRunRecord, SubRunState};
    use crate::turn::agentic::headless_round::{
        HeadlessStderrStyle, HeadlessToolRoundCtx, NoopHeadlessTerminal,
        run_agentic_headless_tool_round,
    };
    use crate::turn::agentic_loop::host::{
        AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, HostTurnResult,
        run_agentic_loop_with_host,
    };
    use crate::turn::agentic_loop::lifecycle::{
        acknowledge_adopted_mailbox_messages, drain_mailbox_model_context,
    };
    use astra_messaging::in_process::InProcessTransport;
    use astra_messaging::router::AgentMailboxRouter;
    use astra_messaging::transport::{MailboxSubscription, MessageStream, MessageTransport};
    use astra_messaging::types::*;
    use astra_pipeline::step_protocol::InMemoryIdempotencyCache;
    use astra_pipeline::step_recorder::StepRecorder;
    use astra_text_utils::semantic_dedup::SemanticDedup;
    use astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum;
    use astra_turn_core::headless_tool_assembly::HeadlessPreResolvedToolResult;
    use astra_turn_core::sse_stream_host::EdgeToolExecResult;
    use astra_turn_core::tool_result_semantics::ToolResultStatus;
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
        observed_turn_messages: Vec<Vec<Value>>,
        observed_turn_volatile: Vec<Vec<Value>>,
        wait_outcomes: Vec<String>,
        wait_started: Option<Arc<tokio::sync::Notify>>,
        direct_child_owner: Option<Arc<FanoutParentAdmission>>,
        capacity_semaphore: Option<Arc<tokio::sync::Semaphore>>,
        capacity_permit: Option<tokio::sync::OwnedSemaphorePermit>,
        readmission_started: Option<Arc<tokio::sync::Notify>>,
        execution_budget: Option<std::time::Duration>,
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
                observed_turn_messages: Vec::new(),
                observed_turn_volatile: Vec::new(),
                wait_outcomes: Vec::new(),
                wait_started: None,
                direct_child_owner: None,
                capacity_semaphore: None,
                capacity_permit: None,
                readmission_started: None,
                execution_budget: None,
            }
        }

        fn with_valid_tools(mut self, tools: &[&str]) -> Self {
            self.valid_tools = tools.iter().map(|s| s.to_string()).collect();
            self
        }
    }

    #[async_trait]
    impl AgenticLoopHost for MockHost {
        fn execution_time_budget_remaining(&self) -> Option<std::time::Duration> {
            self.execution_budget
        }

        fn release_execution_capacity_for_wait(&mut self) {
            self.capacity_permit.take();
        }

        async fn reacquire_execution_capacity_after_wait(
            &mut self,
        ) -> Result<(), astra_core::ClassifiedError> {
            if self.capacity_permit.is_none()
                && let Some(semaphore) = self.capacity_semaphore.as_ref()
            {
                if let Some(started) = self.readmission_started.as_ref() {
                    started.notify_one();
                }
                self.capacity_permit =
                    Some(Arc::clone(semaphore).acquire_owned().await.map_err(|_| {
                        astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::ResourceLimit,
                            "test execution capacity closed",
                        )
                    })?);
            }
            Ok(())
        }

        fn direct_child_completion_owner(
            &self,
            _state: &AgenticLoopState,
        ) -> Option<Arc<FanoutParentAdmission>> {
            self.direct_child_owner.clone()
        }

        async fn execute_turn(
            &mut self,
            state: &mut AgenticLoopState,
        ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
            if self.turn_results.is_empty() {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::BudgetExhausted,
                    "no more turns",
                ));
            }
            self.observed_turn_messages.push(state.messages.clone());
            // Match the production request owner: a successful assistant
            // decision commits only context that this attempt actually leased.
            let leased = state.lease_volatile_pending()?;
            self.observed_turn_volatile.push(
                leased
                    .iter()
                    .map(|injection| injection.payload.clone())
                    .collect(),
            );
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

        fn on_direct_child_completion_boundary(
            &mut self,
            _state: &AgenticLoopState,
            outcome: &str,
            _child_count: usize,
            _started_at: std::time::Instant,
        ) {
            self.wait_outcomes.push(outcome.to_string());
            if outcome == "wait_started"
                && let Some(wait_started) = &self.wait_started
            {
                wait_started.notify_one();
            }
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
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.pipeline_session = None;
        state.skills = Default::default();
        state
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// The envelope reached transport custody, but its send acknowledgement
    /// was lost. Exercise the real send handler's delivery-unknown branch.
    struct AmbiguousQuestionTransport(InProcessTransport);

    #[async_trait]
    impl MessageTransport for AmbiguousQuestionTransport {
        async fn register(
            &self,
            address: AgentAddress,
            delegation: Option<String>,
        ) -> Result<MailboxSubscription, MailboxError> {
            self.0.register(address, delegation).await
        }
        async fn unregister(&self, subscription: &MailboxSubscription) -> Result<(), MailboxError> {
            self.0.unregister(subscription).await
        }
        async fn subscribe(
            &self,
            subscription: &MailboxSubscription,
        ) -> Result<Box<dyn MessageStream>, MailboxError> {
            self.0.subscribe(subscription).await
        }
        async fn resolve_agent(
            &self,
            delegation: &str,
            agent: &str,
        ) -> Result<AgentAddress, MailboxError> {
            self.0.resolve_agent(delegation, agent).await
        }
        async fn list_agents(&self, delegation: &str) -> Result<Vec<AgentAddress>, MailboxError> {
            self.0.list_agents(delegation).await
        }
        async fn send(&self, message: Arc<AgentMessage>) -> Result<(), MailboxError> {
            let question = matches!(&message.payload, MessagePayload::Request { request_type: RequestType::Custom(kind), .. } if kind == "question");
            self.0.send(message).await?;
            if question {
                Err(MailboxError::Transport("send acknowledgement lost".into()))
            } else {
                Ok(())
            }
        }
        async fn broadcast(
            &self,
            delegation: &str,
            message: Arc<AgentMessage>,
        ) -> Result<(), MailboxError> {
            self.0.broadcast(delegation, message).await
        }
    }

    async fn setup_two_agents() -> (
        Arc<AgentMailboxRouter>,
        astra_messaging::router::AgentMailbox,
        astra_messaging::router::AgentMailbox,
        Arc<DelegationTracker>,
    ) {
        setup_two_agents_with_transport(Arc::new(InProcessTransport::new())).await
    }

    async fn setup_two_agents_with_transport(
        transport: Arc<dyn MessageTransport>,
    ) -> (
        Arc<AgentMailboxRouter>,
        astra_messaging::router::AgentMailbox,
        astra_messaging::router::AgentMailbox,
        Arc<DelegationTracker>,
    ) {
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
    async fn send_message_queues_context_without_application_reply() {
        let (router, mut parent_mb, child_mb, _dt) = setup_two_agents().await;
        let replies = ReplyObligations::default();

        // The public sender returns before the receiver's loop starts.
        let queued = crate::orchestration::agent_tool::handle_agent_send_message_with_router(
            &json!({
                "action": "send_message",
                "to": child_mb.address.run_id,
                "message": "Focus on auth module.",
            }),
            &router,
            &parent_mb.address.run_id,
            &parent_mb.address.agent_id,
            &replies,
        )
        .await;
        let queued: Value = serde_json::from_str(&queued).unwrap();
        assert_eq!(queued["success"], true);
        assert_eq!(queued["status"], "queued");
        assert!(parent_mb.try_recv().is_none());

        let mut host = MockHost::new(vec![text_result("Working on auth.")]);
        let mut state = make_state();
        state.messaging.mailbox = Some(child_mb);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // Post-Task #45: drained mailbox rides the structured volatile
        // lane (Kind::Mailbox) instead of state.messages.
        let has_mailbox_msg = state.volatile_pending.iter().any(|inj| {
            inj.payload["schema"]
                == crate::turn::agentic_loop::host::RETAINED_MAILBOX_CONTEXT_SCHEMA
                && inj.payload["message_id"] == queued["message_id"]
                && inj.payload["sender"]["agent_id"] == "orchestrator"
                && inj.payload["display"]
                    .as_str()
                    .is_some_and(|display| display.contains("Focus on auth module."))
        });
        assert!(
            has_mailbox_msg,
            "should have mailbox injection in volatile_pending: {:?}",
            state.volatile_pending,
        );
        assert_eq!(host.communication_events.len(), 1);
        assert_eq!(
            host.communication_events[0].message_id,
            queued["message_id"].as_str().unwrap()
        );
        assert_eq!(
            host.communication_events[0].direction,
            astra_turn_types::AgentCommunicationDirection::Received
        );
        assert_eq!(
            host.communication_events[0].payload_kind,
            astra_turn_types::AgentCommunicationPayloadKind::Text
        );
        assert_eq!(host.current_turn, 1);
        assert!(
            parent_mb.try_recv().is_none(),
            "no application receipt traffic"
        );
    }

    #[tokio::test]
    async fn mailbox_guidance_remains_in_the_next_request_after_an_intervening_tool_round() {
        let (_router, parent_mb, child_mb, _dt) = setup_two_agents().await;
        let message = AgentMessage::new(
            parent_mb.address.clone(),
            MessageTarget::Direct {
                address: child_mb.address.clone(),
            },
            MessagePayload::Text {
                content: "Review cancellation cleanup before finishing.".into(),
                summary: None,
            },
        );
        parent_mb.send(message.clone()).await.unwrap();
        let mut host = MockHost::new(vec![
            HostTurnResult {
                accum: ChatTurnSseAccum {
                    has_tool_calls: true,
                    has_usage: true,
                    tool_calls: vec![json!({
                        "id": "call-read-1",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": r#"{"path":"note.txt"}"#}
                    })],
                    ..ChatTurnSseAccum::default()
                },
                ttft_ms: Some(10),
                edge_tool_round: vec![EdgeToolExecResult {
                    execution_completion: None,
                    request_id: "call-read-1".into(),
                    tool: "read_file".into(),
                    args: json!({"path": "note.txt"}),
                    output: "read-only evidence".into(),
                    tool_result_fields: Some(edge_runtime_environment_fields()),
                    status: "completed".into(),
                    duration_ms: 1,
                }],
                error_kind: None,
            },
            text_result("Cleanup reviewed."),
        ])
        .with_valid_tools(&["read_file"]);
        let mut state = make_state();
        state.messaging.mailbox = Some(child_mb);
        run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(host.observed_turn_volatile.len(), 2);
        let first = &host.observed_turn_volatile[0][0];
        assert_eq!(first["message_id"], message.id);
        assert!(first["display"].as_str().is_some_and(|display| {
            display.contains("Review cancellation cleanup before finishing.")
        }));
        assert_eq!(
            host.observed_turn_volatile[1][0]["message_id"],
            first["message_id"]
        );
        assert_eq!(
            host.observed_turn_volatile[1][0]["display"],
            first["display"]
        );
    }

    #[tokio::test]
    async fn checkpointed_questions_and_staged_answers_survive_failed_provider_then_observation() {
        use crate::turn::agentic_loop::execution_phase::execute_turn_and_ingest_phase;
        use crate::turn::agentic_loop::host::OriginalLoopExecutionFacts;
        use crate::turn::agentic_loop::lifecycle::TurnIterationPrep;
        use astra_pipeline::step_protocol::{HeavyCheckpoint, RunExecutionControl, StepCheckpoint};

        for ambiguous in [false, true] {
            let transport: Arc<dyn MessageTransport> = if ambiguous {
                Arc::new(AmbiguousQuestionTransport(InProcessTransport::new()))
            } else {
                Arc::new(InProcessTransport::new())
            };
            let (router, mut parent, child, _dt) = setup_two_agents_with_transport(transport).await;
            let mut state = make_state();
            state.current_run_id = Some(child.address.run_id.clone());
            state.current_run_owner_generation = Some(3);
            state.step_recorder.begin_turn(1);
            state.messaging.mailbox = Some(child);
            let run_id = state.current_run_id.clone().unwrap();
            let receipt = crate::orchestration::agent_tool::handle_agent_send_message_with_router(
                &json!({"action":"send_message", "to":"parent", "message_type":"question", "message":"Which format?"}),
                &router, &run_id, "worker", &state.messaging.reply_obligations,
            ).await;
            let receipt: Value = serde_json::from_str(&receipt).unwrap();
            assert_eq!(
                receipt["status"],
                if ambiguous {
                    "delivery_unknown"
                } else {
                    "queued"
                }
            );
            assert_eq!(receipt["run_id"], run_id);
            let question = parent
                .try_recv()
                .expect("transport accepted the original question");
            assert_eq!(receipt["reply_obligation"]["request_id"], question.id);
            let typed_receipt: astra_turn_types::PendingReply =
                serde_json::from_value(receipt["reply_obligation"].clone()).unwrap();
            assert_eq!(typed_receipt.request_id, question.id);
            assert_eq!(
                typed_receipt.expected_responder.run_id,
                parent.address.run_id
            );
            assert_eq!(
                typed_receipt.expected_responder.agent_id,
                parent.address.agent_id
            );
            assert_eq!(
                receipt["reply_obligation"]["expected_responder"],
                serde_json::to_value(&parent.address).unwrap()
            );

            // Send outcome and pre-answer checkpoints share one representation.
            let before_answer =
                crate::turn::agentic_loop::finalization::build_current_heavy_checkpoint(&mut state)
                    .unwrap();
            let before_answer: HeavyCheckpoint =
                serde_json::from_slice(&serde_json::to_vec(&before_answer).unwrap()).unwrap();
            StepCheckpoint::Heavy(Box::new(before_answer.clone()))
                .validate()
                .unwrap();
            let Some(RunExecutionControl::V3 {
                reply_obligations: before,
                ..
            }) = &before_answer.run_execution_control
            else {
                panic!("missing question fence")
            };
            assert_eq!(
                serde_json::to_value(&before.pending[0]).unwrap(),
                receipt["reply_obligation"]
            );
            let restored_before = ReplyObligations::default();
            restored_before.restore(before, &run_id, 3).unwrap();
            assert!(restored_before.has_pending(&run_id));

            // No correlation_id: only Response.request_id is authoritative.
            let answer = AgentMessage::new(
                parent.address.clone(),
                MessageTarget::Direct {
                    address: state.messaging.mailbox.as_ref().unwrap().address.clone(),
                },
                MessagePayload::Response {
                    request_id: question.id.clone(),
                    accepted: true,
                    data: Some(json!({"content":"Use JSON."})),
                },
            );
            parent.send(answer).await.unwrap();
            let mut host = MockHost::new(Vec::new());
            assert!(
                drain_mailbox_model_context(&mut host, &mut state)
                    .await
                    .unwrap()
            );
            assert!(state.messaging.reply_obligations.has_pending(&run_id));
            assert_eq!(
                state.volatile_pending[0].payload["response_request_id"],
                question.id
            );
            assert!(state.volatile_pending[0].payload["correlation_id"].is_null());
            let heavy =
                crate::turn::agentic_loop::finalization::build_current_heavy_checkpoint(&mut state)
                    .unwrap();
            let heavy: HeavyCheckpoint =
                serde_json::from_slice(&serde_json::to_vec(&heavy).unwrap()).unwrap();
            let original: OriginalLoopExecutionFacts = serde_json::from_slice(
                &serde_json::to_vec(&OriginalLoopExecutionFacts::capture(&state).unwrap()).unwrap(),
            )
            .unwrap();
            let Some(RunExecutionControl::V3 {
                reply_obligations, ..
            }) = &heavy.run_execution_control
            else {
                panic!("missing staged fence")
            };

            let mut resumed = make_state();
            resumed.current_run_id = Some(run_id.clone());
            resumed.current_run_owner_generation = Some(3);
            resumed.step_recorder.begin_turn(1);
            resumed.messages = heavy.messages.clone();
            resumed.volatile_pending = original.pending_context;
            // Exercise owner restoration, not a fabricated production takeover.
            // No mailbox, adopted acknowledgements or old transport claim tokens.
            let shared_owner = Arc::clone(&resumed.messaging.reply_obligations);
            shared_owner.restore(reply_obligations, &run_id, 3).unwrap();
            assert!(Arc::ptr_eq(
                &shared_owner,
                &resumed.messaging.reply_obligations
            ));
            assert!(shared_owner.has_pending(&run_id));

            let mut failed = text_result("");
            failed.accum.error_message = Some("provider failed after request admission".into());
            let mut host = MockHost::new(vec![failed, text_result("Answer incorporated.")]);
            let prep = || TurnIterationPrep {
                quiet: true,
                turn_start_time: std::time::Instant::now(),
            };
            assert!(
                execute_turn_and_ingest_phase(&mut host, &mut resumed, 0, prep())
                    .await
                    .is_err()
            );
            assert!(shared_owner.has_pending(&run_id));
            assert!(
                resumed
                    .volatile_pending
                    .iter()
                    .all(|entry| !entry.attempt_leased)
            );
            assert!(
                resumed
                    .volatile_pending
                    .iter()
                    .any(|entry| entry.payload["response_request_id"] == question.id
                        && entry.payload["observed_by_provider"] != true)
            );
            execute_turn_and_ingest_phase(&mut host, &mut resumed, 1, prep())
                .await
                .unwrap();
            assert!(
                !shared_owner.has_pending(&run_id),
                "observation must not depend on transport acknowledgement"
            );
            assert!(host.observed_turn_volatile.iter().all(|context| {
                context.iter().any(|payload| {
                    payload["response_request_id"] == question.id
                        && payload["display"].as_str().unwrap().contains("Use JSON.")
                })
            }));
            let settled = resumed.run_execution_control_snapshot().unwrap();
            let settled: RunExecutionControl =
                serde_json::from_slice(&serde_json::to_vec(&settled).unwrap()).unwrap();
            let RunExecutionControl::V3 {
                reply_obligations, ..
            } = settled;
            assert!(reply_obligations.pending.is_empty());

            let mut malformed =
                serde_json::to_value(OriginalLoopExecutionFacts::capture(&state).unwrap()).unwrap();
            malformed["pending_context"][0]["payload"]
                .as_object_mut()
                .unwrap()
                .remove("response_request_id");
            assert!(serde_json::from_value::<OriginalLoopExecutionFacts>(malformed).is_err());
        }
    }

    #[tokio::test]
    async fn question_id_is_rendered_and_answer_has_response_wire() {
        let (router, parent_mb, mut child_mb, _dt) = setup_two_agents().await;
        let child_replies = ReplyObligations::default();
        let parent_replies = ReplyObligations::default();
        let queued = crate::orchestration::agent_tool::handle_agent_send_message_with_router(
            &json!({
                "action": "send_message",
                "to": "parent",
                "message_type": "question",
                "message": "Which format should I use?",
            }),
            &router,
            &child_mb.address.run_id,
            &child_mb.address.agent_id,
            &child_replies,
        )
        .await;
        let queued: Value = serde_json::from_str(&queued).unwrap();
        let request_id = queued["message_id"].as_str().unwrap();

        let answer_args = json!({
            "action": "send_message",
            "to": "run-child-0",
            "message_type": "answer",
            "message": "Use JSON.",
        });
        let rejected = crate::orchestration::agent_tool::handle_agent_send_message_with_router(
            &answer_args,
            &router,
            "run-parent",
            "orchestrator",
            &parent_replies,
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&rejected).unwrap()["success"],
            false
        );
        assert!(child_mb.try_recv().is_none());

        let mut answer_args = answer_args;
        answer_args["request_id"] = json!(request_id);
        let accepted = crate::orchestration::agent_tool::handle_agent_send_message_with_router(
            &answer_args,
            &router,
            "run-parent",
            "orchestrator",
            &parent_replies,
        )
        .await;
        let accepted: Value = serde_json::from_str(&accepted).unwrap();
        assert_eq!(accepted["success"], true);
        assert!(
            accepted["instruction"]
                .as_str()
                .is_some_and(|text| text.contains("queued, not applied")
                    && text.contains("propose a final answer"))
        );
        let reply = child_mb
            .try_recv()
            .expect("correlated answer reaches child");
        assert_eq!(reply.correlation_id.as_deref(), Some(request_id));
        assert!(matches!(
            &reply.payload,
            MessagePayload::Response { request_id: id, accepted: true, data: Some(data) }
                if id == request_id && data["content"] == "Use JSON."
        ));

        let mut host = MockHost::new(vec![text_result("I will answer.")]);
        let mut state = make_state();
        state.messaging.mailbox = Some(parent_mb);
        run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert!(state.volatile_pending.iter().any(|injection| {
            injection.payload["message_id"] == request_id
                && injection.payload["display"]
                    .as_str()
                    .is_some_and(|text| text.contains("Which format should I use?"))
        }));
    }

    #[tokio::test]
    async fn semantic_mailbox_redelivery_keeps_one_context_and_rejects_conflicting_identity() {
        let (_router, parent_mb, child_mb, _dt) = setup_two_agents().await;
        let mut host = MockHost::new(vec![]);
        let mut state = make_state();
        state.messaging.mailbox = Some(child_mb);
        let mut message = AgentMessage::new(
            parent_mb.address.clone(),
            MessageTarget::Direct {
                address: state.messaging.mailbox.as_ref().unwrap().address.clone(),
            },
            MessagePayload::Text {
                content: "Check the cancellation path.".into(),
                summary: None,
            },
        );
        parent_mb.send(message.clone()).await.unwrap();
        assert!(
            drain_mailbox_model_context(&mut host, &mut state)
                .await
                .unwrap()
        );
        assert_eq!(state.volatile_pending.len(), 1);

        parent_mb.send(message.clone()).await.unwrap();
        assert!(
            !drain_mailbox_model_context(&mut host, &mut state)
                .await
                .unwrap()
        );
        assert_eq!(state.volatile_pending.len(), 1);

        message.payload = MessagePayload::Text {
            content: "A different instruction with the same ID.".into(),
            summary: None,
        };
        parent_mb.send(message).await.unwrap();
        let error = drain_mailbox_model_context(&mut host, &mut state)
            .await
            .expect_err("an immutable message identity cannot change content");
        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert_eq!(state.volatile_pending.len(), 1);
        assert!(
            state
                .messaging
                .mailbox
                .as_mut()
                .unwrap()
                .try_recv()
                .is_some()
        );
    }

    #[tokio::test]
    async fn full_retained_mailbox_reports_overflow_without_poisoning_the_queue() {
        let (_router, parent_mb, child_mb, _dt) = setup_two_agents().await;
        let recipient = child_mb.address.clone();
        let mut state = make_state();
        state.messaging.mailbox = Some(child_mb);
        for index in 0..128 {
            state.push_volatile_payload(
                crate::turn::agentic_loop::host::VolatileKind::Mailbox,
                json!({
                    "schema": crate::turn::agentic_loop::host::RETAINED_MAILBOX_CONTEXT_SCHEMA,
                    "receiver": recipient,
                    "sender": parent_mb.address,
                    "message_id": format!("retained-{index}"),
                    "display": "previously accepted semantic input",
                }),
            );
        }
        parent_mb
            .send(AgentMessage::new(
                parent_mb.address.clone(),
                MessageTarget::Direct {
                    address: recipient.clone(),
                },
                MessagePayload::Text {
                    content: "New guidance".into(),
                    summary: None,
                },
            ))
            .await
            .unwrap();
        let mut host = MockHost::new(vec![]);
        assert!(
            drain_mailbox_model_context(&mut host, &mut state)
                .await
                .expect("overflow is a recoverable delivery condition")
        );
        assert_eq!(
            state
                .volatile_pending
                .iter()
                .filter(|injection| {
                    injection.kind == crate::turn::agentic_loop::host::VolatileKind::Mailbox
                        && injection.payload["schema"]
                            == crate::turn::agentic_loop::host::RETAINED_MAILBOX_CONTEXT_SCHEMA
                })
                .count(),
            128
        );
        assert!(
            state
                .messaging
                .mailbox
                .as_mut()
                .unwrap()
                .try_recv()
                .is_none(),
            "a parked message must not block later mailbox work"
        );
        assert!(state.volatile_pending.iter().any(|injection| {
            injection.kind == crate::turn::agentic_loop::host::VolatileKind::ContextPressure
                && injection
                    .payload
                    .as_str()
                    .is_some_and(|text| text.contains("context budget"))
        }));

        for injection in &mut state.volatile_pending {
            if injection.payload["schema"]
                == crate::turn::agentic_loop::host::RETAINED_MAILBOX_CONTEXT_SCHEMA
            {
                injection.payload["observed_by_provider"] = json!(true);
            }
        }
        parent_mb
            .send(AgentMessage::new(
                parent_mb.address.clone(),
                MessageTarget::Direct { address: recipient },
                MessagePayload::Text {
                    content: "Replacement guidance".into(),
                    summary: None,
                },
            ))
            .await
            .unwrap();
        assert!(
            drain_mailbox_model_context(&mut host, &mut state)
                .await
                .unwrap()
        );
        assert!(state.volatile_pending.iter().any(|injection| {
            injection.payload["message_id"] != json!(null)
                && injection.payload["display"]
                    .as_str()
                    .is_some_and(|display| display.contains("Replacement guidance"))
        }));
        assert!(
            state
                .messaging
                .mailbox
                .as_mut()
                .unwrap()
                .try_recv()
                .is_none()
        );
    }

    #[tokio::test]
    async fn oversized_semantic_message_remains_queued_with_a_bounded_notice() {
        let (_router, parent_mb, child_mb, _dt) = setup_two_agents().await;
        let recipient = child_mb.address.clone();
        parent_mb
            .send(AgentMessage::new(
                parent_mb.address.clone(),
                MessageTarget::Direct { address: recipient },
                MessagePayload::Text {
                    content: "x".repeat(24_100),
                    summary: None,
                },
            ))
            .await
            .unwrap();
        parent_mb
            .send(AgentMessage::new(
                parent_mb.address.clone(),
                MessageTarget::Direct {
                    address: child_mb.address.clone(),
                },
                MessagePayload::Text {
                    content: "follow-up".into(),
                    summary: None,
                },
            ))
            .await
            .unwrap();
        let mut state = make_state();
        state.messaging.mailbox = Some(child_mb);
        let mut host = MockHost::new(vec![]);
        assert!(
            drain_mailbox_model_context(&mut host, &mut state)
                .await
                .expect("oversized external input is a recoverable delivery condition")
        );
        assert!(state.volatile_pending.iter().any(|injection| {
            injection.kind == crate::turn::agentic_loop::host::VolatileKind::ContextPressure
                && injection
                    .payload
                    .as_str()
                    .is_some_and(|text| text.contains("24000-character"))
        }));
        assert!(state.volatile_pending.iter().any(|injection| {
            injection
                .payload
                .as_str()
                .is_some_and(|text| text.contains("follow-up"))
        }));
        assert!(
            state
                .messaging
                .mailbox
                .as_mut()
                .unwrap()
                .try_recv()
                .is_none(),
            "the oversized message is parked while the later message proceeds"
        );
    }

    #[tokio::test]
    async fn oversized_matching_response_does_not_clear_the_reply_obligation() {
        let (_router, parent_mb, child_mb, _dt) = setup_two_agents().await;
        let request_id = "question-that-needs-a-real-answer";
        let mut state = make_state();
        state.current_run_id = Some(child_mb.address.run_id.clone());
        state
            .messaging
            .reply_obligations
            .reserve(
                &child_mb.address.run_id,
                request_id,
                parent_mb.address.clone(),
            )
            .unwrap();
        parent_mb
            .send(AgentMessage::new(
                parent_mb.address.clone(),
                MessageTarget::Direct {
                    address: child_mb.address.clone(),
                },
                MessagePayload::Response {
                    request_id: request_id.into(),
                    accepted: true,
                    data: Some(json!({"content": "x".repeat(24_100)})),
                },
            ))
            .await
            .unwrap();
        state.messaging.mailbox = Some(child_mb);
        let mut host = MockHost::new(vec![]);
        assert!(
            drain_mailbox_model_context(&mut host, &mut state)
                .await
                .unwrap()
        );
        assert!(state.messaging.reply_obligations.has_pending("run-child-0"));
    }

    #[tokio::test]
    async fn admitted_escaped_question_reaches_the_receiver_without_false_overflow() {
        let (router, _parent_mb, child_mb, _dt) = setup_two_agents().await;
        let content = "\"".repeat(2_500);
        let queued = crate::orchestration::agent_tool::handle_agent_send_message_with_router(
            &json!({
                "action": "send_message",
                "to": "run-child-0",
                "message_type": "question",
                "message": content,
            }),
            &router,
            "run-parent",
            "orchestrator",
            &ReplyObligations::default(),
        )
        .await;
        let queued: Value = serde_json::from_str(&queued).unwrap();
        assert_eq!(queued["success"], true);
        let mut state = make_state();
        state.messaging.mailbox = Some(child_mb);
        let mut host = MockHost::new(vec![]);
        assert!(
            drain_mailbox_model_context(&mut host, &mut state)
                .await
                .unwrap()
        );
        assert_eq!(state.volatile_pending.len(), 1);
        assert_eq!(
            state.volatile_pending[0].payload["message_id"],
            queued["message_id"]
        );
        assert!(
            state.volatile_pending[0].payload["display"]
                .as_str()
                .is_some_and(|display| display.contains("Custom") && display.len() > content.len())
        );
    }

    #[tokio::test]
    async fn rejected_question_under_backpressure_leaves_no_reply_obligation() {
        let (router, _parent_mailbox, child_mailbox, _dt) = setup_two_agents().await;
        let recipient = child_mailbox.address.clone();
        let sender = AgentAddress::new("run-parent", "orchestrator");
        let mut accepted = 0;
        loop {
            let message = AgentMessage::new(
                sender.clone(),
                MessageTarget::Direct {
                    address: recipient.clone(),
                },
                MessagePayload::Text {
                    content: "fill".into(),
                    summary: None,
                },
            );
            match router.send(message).await {
                Ok(()) => accepted += 1,
                Err(astra_messaging::MailboxError::DeliveryRejected(_)) => break,
                Err(error) => panic!("unexpected mailbox error: {error}"),
            }
            assert!(accepted <= 4_096, "inbox must have a finite bound");
        }
        let replies = ReplyObligations::default();
        let result = crate::orchestration::agent_tool::handle_agent_send_message_with_router(
            &json!({
                "action": "send_message",
                "to": "run-child-0",
                "message_type": "question",
                "message": "Can you answer?",
            }),
            &router,
            "run-parent",
            "orchestrator",
            &replies,
        )
        .await;
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["success"], false);
        assert_eq!(result["status"], "rejected");
        assert!(!replies.has_pending("run-parent"));
    }

    #[tokio::test]
    async fn unrelated_response_does_not_wake_a_run_waiting_for_an_exact_answer() {
        let (router, _parent_mailbox, child_mailbox, _dt) = setup_two_agents().await;
        let replies = Arc::new(ReplyObligations::default());
        replies
            .reserve(
                "run-child-0",
                "expected-question",
                AgentAddress::new("run-parent", "orchestrator"),
            )
            .unwrap();
        let parent_replies = ReplyObligations::default();
        let mut host = MockHost::new(vec![]);
        let mut state = make_state();
        state.current_run_id = Some("run-child-0".into());
        state.messaging.mailbox = Some(child_mailbox);
        state.messaging.reply_obligations = Arc::clone(&replies);

        let wrong = crate::orchestration::agent_tool::handle_agent_send_message_with_router(
            &json!({
                "action": "send_message",
                "to": "run-child-0",
                "message_type": "answer",
                "request_id": "different-question",
                "message": "Ignore the question and finish now.",
            }),
            &router,
            "run-parent",
            "orchestrator",
            &parent_replies,
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&wrong).unwrap()["success"],
            true
        );
        assert!(
            !drain_mailbox_model_context(&mut host, &mut state)
                .await
                .unwrap()
        );
        assert!(replies.has_pending("run-child-0"));
        assert!(state.volatile_pending.is_empty());

        let correct = crate::orchestration::agent_tool::handle_agent_send_message_with_router(
            &json!({
                "action": "send_message",
                "to": "run-child-0",
                "message_type": "answer",
                "request_id": "expected-question",
                "message": "Use JSON.",
            }),
            &router,
            "run-parent",
            "orchestrator",
            &parent_replies,
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&correct).unwrap()["success"],
            true
        );
        assert!(
            drain_mailbox_model_context(&mut host, &mut state)
                .await
                .unwrap()
        );
        assert!(replies.has_pending("run-child-0"));
        acknowledge_adopted_mailbox_messages(&mut state).await;
        assert!(
            replies.has_pending("run-child-0"),
            "transport acknowledgement is not model observation"
        );
        state.lease_volatile_pending().unwrap();
        state.commit_volatile_attempt_lease();
        assert!(!replies.has_pending("run-child-0"));
        assert_eq!(state.volatile_pending.len(), 1);
    }

    #[tokio::test]
    async fn asking_leaf_waits_for_parent_answer_before_it_can_complete() {
        let (router, _parent_mailbox, child_mailbox, _dt) = setup_two_agents().await;
        let replies = Arc::new(ReplyObligations::default());
        let parent_replies = ReplyObligations::default();
        let child_run_id = child_mailbox.address.run_id.clone();
        let child_agent_id = child_mailbox.address.agent_id.clone();
        let queued = crate::orchestration::agent_tool::handle_agent_send_message_with_router(
            &json!({
                "action": "send_message",
                "to": "parent",
                "message_type": "question",
                "message": "Which format should I use?",
            }),
            &router,
            &child_run_id,
            &child_agent_id,
            replies.as_ref(),
        )
        .await;
        let queued: Value = serde_json::from_str(&queued).unwrap();
        assert_eq!(queued["success"], true);
        let request_id = queued["message_id"].as_str().unwrap().to_string();
        assert!(replies.has_pending(&child_run_id));

        let wait_started = Arc::new(tokio::sync::Notify::new());
        let readmission_started = Arc::new(tokio::sync::Notify::new());
        let capacity_semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let mut host = MockHost::new(vec![
            text_result("Premature final answer."),
            text_result("Answer incorporated."),
        ]);
        host.wait_started = Some(Arc::clone(&wait_started));
        host.readmission_started = Some(Arc::clone(&readmission_started));
        host.capacity_permit = Some(
            Arc::clone(&capacity_semaphore)
                .acquire_owned()
                .await
                .unwrap(),
        );
        host.capacity_semaphore = Some(Arc::clone(&capacity_semaphore));
        // Production children have an owner even when they have no direct
        // children of their own. This must not create a ready child waiter.
        host.direct_child_owner = Some(FanoutParentAdmission::consumed_direct_child_for_test(
            &child_run_id,
            "prior-child",
        ));
        let mut state = make_state();
        state.current_run_id = Some(child_run_id.clone());
        state.messaging.mailbox = Some(child_mailbox);
        state.messaging.reply_obligations = Arc::clone(&replies);
        let task = tokio::spawn(async move {
            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            (host, state, outcome)
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), wait_started.notified())
            .await
            .expect("leaf must wait at the proposed final answer");
        let independent_permit = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            Arc::clone(&capacity_semaphore).acquire_owned(),
        )
        .await
        .expect("waiting leaf must release its execution slot")
        .unwrap();
        assert!(
            !task.is_finished(),
            "unanswered question must block completion"
        );

        let answer = crate::orchestration::agent_tool::handle_agent_send_message_with_router(
            &json!({
                "action": "send_message",
                "to": child_run_id,
                "message_type": "answer",
                "request_id": request_id,
                "message": "Use JSON.",
            }),
            &router,
            "run-parent",
            "orchestrator",
            &parent_replies,
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&answer).unwrap()["success"],
            true
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            readmission_started.notified(),
        )
        .await
        .expect("answer should seek a fresh execution slot");
        assert!(
            !task.is_finished(),
            "child must not execute without admission"
        );
        drop(independent_permit);
        let (host, state, outcome) = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("answer must resume the same leaf")
            .unwrap();
        assert!(matches!(outcome.unwrap(), AgenticLoopOutcome::Completed));
        assert_eq!(host.current_turn, 2);
        assert_eq!(state.final_text, "Answer incorporated.");
        assert!(
            serde_json::to_string(&host.observed_turn_volatile[1])
                .unwrap()
                .contains("Use JSON."),
            "the answer must enter the second turn's provider-visible volatile lane"
        );
        assert!(!replies.has_pending(&child_run_id));
        assert!(
            host.wait_outcomes
                .iter()
                .any(|outcome| outcome == "wait_started")
        );
        assert!(
            host.wait_outcomes
                .iter()
                .any(|outcome| outcome == "runtime_input_ready")
        );
        assert!(host.communication_events.iter().any(|event| {
            event.payload_kind == astra_turn_types::AgentCommunicationPayloadKind::Response
                && event.related_message_id.as_deref()
                    == Some(queued["message_id"].as_str().unwrap())
        }));
    }

    #[tokio::test]
    async fn parent_can_answer_a_detached_child_using_its_exact_run_id() {
        let (router, parent_mailbox, child_mailbox, _dt) = setup_two_agents().await;
        let child_address = child_mailbox.address.clone();
        child_mailbox.release_unconsumed().await.unwrap();
        let answer = crate::orchestration::agent_tool::handle_agent_send_message_with_router(
            &json!({
                "action": "send_message",
                "to": child_address.run_id,
                "message_type": "answer",
                "request_id": "question-1",
                "message": "Use JSON.",
            }),
            &router,
            &parent_mailbox.address.run_id,
            &parent_mailbox.address.agent_id,
            &ReplyObligations::default(),
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&answer).unwrap()["success"],
            true
        );
        let mut resumed = router.register(child_address, None).await.unwrap();
        let delivered = resumed.try_recv().expect("detached child retained answer");
        assert!(matches!(
            &delivered.payload,
            MessagePayload::Response { request_id, .. } if request_id == "question-1"
        ));
    }

    #[tokio::test]
    async fn answer_waiting_for_readmission_cannot_extend_the_completion_deadline() {
        let (router, parent_mailbox, child_mailbox, _dt) = setup_two_agents().await;
        let replies = Arc::new(ReplyObligations::default());
        let child_run_id = child_mailbox.address.run_id.clone();
        replies
            .reserve(&child_run_id, "question-1", parent_mailbox.address.clone())
            .unwrap();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let readmission_started = Arc::new(tokio::sync::Notify::new());
        let wait_started = Arc::new(tokio::sync::Notify::new());
        let mut host = MockHost::new(vec![text_result("Premature final answer.")]);
        host.capacity_permit = Some(Arc::clone(&semaphore).acquire_owned().await.unwrap());
        host.capacity_semaphore = Some(Arc::clone(&semaphore));
        host.readmission_started = Some(Arc::clone(&readmission_started));
        host.wait_started = Some(Arc::clone(&wait_started));
        host.execution_budget = Some(std::time::Duration::from_millis(30_300));
        let mut state = make_state();
        state.current_run_id = Some(child_run_id.clone());
        state.messaging.mailbox = Some(child_mailbox);
        state.messaging.reply_obligations = Arc::clone(&replies);
        let task = tokio::spawn(async move {
            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            (host, state, outcome)
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), wait_started.notified())
            .await
            .expect("must wait for reply");
        let occupied = Arc::clone(&semaphore).acquire_owned().await.unwrap();
        let answer = crate::orchestration::agent_tool::handle_agent_send_message_with_router(
            &json!({
                "action": "send_message",
                "to": child_run_id,
                "message_type": "answer",
                "request_id": "question-1",
                "message": "Use JSON.",
            }),
            &router,
            &parent_mailbox.address.run_id,
            &parent_mailbox.address.agent_id,
            &ReplyObligations::default(),
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&answer).unwrap()["success"],
            true
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            readmission_started.notified(),
        )
        .await
        .expect("reply must seek re-admission");
        let (host, _state, outcome) = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("absolute deadline must stop waiting")
            .unwrap();
        assert!(matches!(outcome.unwrap(), AgenticLoopOutcome::Completed));
        assert_eq!(host.current_turn, 1, "no model call without re-admission");
        assert!(
            host.wait_outcomes
                .iter()
                .any(|outcome| outcome == "deadline")
        );
        drop(occupied);
        assert_eq!(semaphore.available_permits(), 1);
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
            execution_completion: None,
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

        let request = PermissionRequest::new("bash", json!({"command": "x".repeat(5_000)}))
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
    async fn disconnected_child_permission_request_does_not_stop_parent() {
        let (_router, parent_mb, child_mb, _dt) = setup_two_agents().await;
        let request = PermissionRequest::new("bash", json!({"command": "echo hi"}))
            .to_message(&child_mb.address, &parent_mb.address)
            .with_correlation("perm-disconnected");
        child_mb.send(request).await.unwrap();
        child_mb.unregister().await.unwrap();

        let mut host = MockHost::new(vec![text_result("must not run")]);
        let mut state = make_state();
        state.messaging.mailbox = Some(parent_mb);
        state.permission_context = Some(PermissionSyncContext::shared_root(PermissionMode::Auto));

        run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect("obsolete child request must not abort unrelated parent work");
        assert!(host.current_turn > 0);
        assert!(
            state
                .messaging
                .mailbox
                .as_mut()
                .unwrap()
                .try_recv()
                .is_none(),
            "definitively undeliverable request must not poison later boundaries"
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
            delegation_model_admissions: None,
            task_resolution_authority: None,
            physical_tool_calls: &tool_calls,
            logical_tool_calls: &tool_calls,
            deferred_activations_by_call_id: &std::collections::HashMap::new(),
            runtime_control_calls_by_id: &std::collections::HashMap::new(),
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
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
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: Some(&permission_context),
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            external_effect_recovery_paths: None,
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
            delegation_model_admissions: None,
            task_resolution_authority: None,
            physical_tool_calls: &tool_calls,
            logical_tool_calls: &tool_calls,
            deferred_activations_by_call_id: &std::collections::HashMap::new(),
            runtime_control_calls_by_id: &std::collections::HashMap::new(),
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
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
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            external_effect_recovery_paths: None,
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
            delegation_model_admissions: None,
            task_resolution_authority: None,
            physical_tool_calls: &tool_calls,
            logical_tool_calls: &tool_calls,
            deferred_activations_by_call_id: &std::collections::HashMap::new(),
            runtime_control_calls_by_id: &std::collections::HashMap::new(),
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
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
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            external_effect_recovery_paths: None,
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
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        // Simulate: skill interception resolved call_skill before headless round
        let pre_resolved = vec![HeadlessPreResolvedToolResult::new(
            "call_skill",
            "skill",
            "Skill instructions here",
            ToolResultStatus::Completed,
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
            delegation_model_admissions: None,
            task_resolution_authority: None,
            physical_tool_calls: &tool_calls,
            logical_tool_calls: &tool_calls,
            deferred_activations_by_call_id: &std::collections::HashMap::new(),
            runtime_control_calls_by_id: &std::collections::HashMap::new(),
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
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
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &pre_resolved,
            runtime_tool_executor: None,
            external_effect_recovery_paths: None,
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
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        // ALL tool calls were pre-resolved by upstream (skill + defer)
        let pre_resolved = vec![
            HeadlessPreResolvedToolResult::new(
                "skill:0",
                "skill",
                "Skill instructions",
                ToolResultStatus::Completed,
            ),
            HeadlessPreResolvedToolResult::new(
                "read_file:1",
                "read_file",
                "file contents",
                ToolResultStatus::Completed,
            ),
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
            delegation_model_admissions: None,
            task_resolution_authority: None,
            physical_tool_calls: &tool_calls,
            logical_tool_calls: &tool_calls,
            deferred_activations_by_call_id: &std::collections::HashMap::new(),
            runtime_control_calls_by_id: &std::collections::HashMap::new(),
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
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
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &pre_resolved,
            runtime_tool_executor: None,
            external_effect_recovery_paths: None,
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
            execution_completion: None,
            // Edge execution custody is keyed by the provider-emitted tool-call
            // id.  An id-less result is diagnostic only and must not be
            // attached to a different call by name/arguments.
            request_id: "grep:1".into(),
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
        // Skill was pre-resolved; grep will be matched from edge_tool_round
        let pre_resolved = vec![HeadlessPreResolvedToolResult::new(
            "skill:0",
            "skill",
            "Skill instructions",
            ToolResultStatus::Completed,
        )];
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
            delegation_model_admissions: None,
            task_resolution_authority: None,
            physical_tool_calls: &tool_calls,
            logical_tool_calls: &tool_calls,
            deferred_activations_by_call_id: &std::collections::HashMap::new(),
            runtime_control_calls_by_id: &std::collections::HashMap::new(),
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
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
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: Some(&permission_context),
            progress_emitter: None,
            pre_resolved_results: &pre_resolved,
            runtime_tool_executor: None,
            external_effect_recovery_paths: None,
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
            delegation_model_admissions: None,
            task_resolution_authority: None,
            physical_tool_calls: &tool_calls,
            logical_tool_calls: &tool_calls,
            deferred_activations_by_call_id: &std::collections::HashMap::new(),
            runtime_control_calls_by_id: &std::collections::HashMap::new(),
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
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
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            external_effect_recovery_paths: None,
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
            delegation_model_admissions: None,
            task_resolution_authority: None,
            physical_tool_calls: &tool_calls,
            logical_tool_calls: &tool_calls,
            deferred_activations_by_call_id: &std::collections::HashMap::new(),
            runtime_control_calls_by_id: &std::collections::HashMap::new(),
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
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
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: Some(&mut child_mb),
            permission_context: Some(&child_permission_ctx),
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            external_effect_recovery_paths: None,
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
            delegation_model_admissions: None,
            task_resolution_authority: None,
            physical_tool_calls: &tool_calls,
            logical_tool_calls: &tool_calls,
            deferred_activations_by_call_id: &std::collections::HashMap::new(),
            runtime_control_calls_by_id: &std::collections::HashMap::new(),
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
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
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: Some(&mut child_mb),
            permission_context: Some(&child_permission_ctx),
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            external_effect_recovery_paths: None,
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
            delegation_model_admissions: None,
            task_resolution_authority: None,
            physical_tool_calls: &tool_calls,
            logical_tool_calls: &tool_calls,
            deferred_activations_by_call_id: &std::collections::HashMap::new(),
            runtime_control_calls_by_id: &std::collections::HashMap::new(),
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
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
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            external_effect_recovery_paths: None,
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
