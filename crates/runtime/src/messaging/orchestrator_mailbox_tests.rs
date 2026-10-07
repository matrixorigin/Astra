//! Tests for owned parent mailboxes in team delegations.
//!
//! All tests are model-free: they use `SubRunExecutor` mocks that exercise
//! the real `DelegationEngine`, `AgentMailboxRouter`, and `InProcessTransport`.

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio::sync::RwLock;

    use astra_services::coordination::{
        AgentProfile, AgentProfileRegistry, AgentResult, AgentTier, AggregationStrategy,
        CoordinationPattern, DelegationRequest,
    };
    use astra_services::runs::InMemoryRunStateStore;

    use crate::server::delegation::engine::{
        DelegationEngine, DelegationTracker, SubRunConfig, SubRunExecutor,
    };
    use crate::server::run::engine::RunEngine;
    use astra_messaging::in_process::InProcessTransport;
    use astra_messaging::router::{AgentMailbox, AgentMailboxRouter};
    use astra_messaging::types::*;

    // ── Executor that records whether send_progress succeeds ────────────

    struct ProgressReportingExecutor {
        results: Arc<tokio::sync::Mutex<Vec<(String, Result<(), String>)>>>,
    }

    impl ProgressReportingExecutor {
        fn new() -> (
            Self,
            Arc<tokio::sync::Mutex<Vec<(String, Result<(), String>)>>>,
        ) {
            let results = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            (
                Self {
                    results: results.clone(),
                },
                results,
            )
        }
    }

    #[async_trait]
    impl SubRunExecutor for ProgressReportingExecutor {
        async fn execute(
            &self,
            config: SubRunConfig,
        ) -> Result<(AgentResult, Option<crate::orchestration::SpawnRunFrontier>), String> {
            let agent_id = config.agent_profile.agent_id.clone();
            let run_id = config.run_id.clone();

            let send_result = if let Some(ref mailbox) = config.mailbox {
                match mailbox
                    .send_progress(0, 1, "turn_complete", Some("test".into()))
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(e) => Err(format!("{e}")),
                }
            } else {
                Err("no mailbox".into())
            };

            self.results
                .lock()
                .await
                .push((agent_id.clone(), send_result));

            Ok((
                AgentResult {
                    agent_id,
                    run_id,
                    status: "completed".into(),
                    output: Some("done".into()),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
                None,
            ))
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────────

    fn setup_profiles() -> Arc<RwLock<AgentProfileRegistry>> {
        let mut reg = AgentProfileRegistry::new();
        reg.register(AgentProfile::new(
            "orch",
            "Orchestrator",
            AgentTier::Orchestrator,
        ))
        .unwrap();
        reg.register(AgentProfile::new(
            "team-review-producer",
            "Producer",
            AgentTier::System,
        ))
        .unwrap();
        reg.register(AgentProfile::new(
            "team-review-reviewer",
            "Reviewer",
            AgentTier::System,
        ))
        .unwrap();
        reg.register(AgentProfile::new("worker-a", "Worker A", AgentTier::System))
            .unwrap();
        reg.register(AgentProfile::new("worker-b", "Worker B", AgentTier::System))
            .unwrap();
        Arc::new(RwLock::new(reg))
    }

    fn make_request(
        pattern: CoordinationPattern,
        parent_run_id: &str,
        delegation_id: &str,
    ) -> DelegationRequest {
        DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: delegation_id.into(),
            parent_run_id: parent_run_id.into(),
            task: "test task".into(),
            pattern,
            user_id: "test-user".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: {
                let mut ctx = HashMap::new();
                ctx.insert(
                    "session_id".into(),
                    serde_json::Value::String("test-session".into()),
                );
                ctx
            },
            execution_metadata: None,
        }
    }

    struct TestHarness {
        engine: DelegationEngine,
        run_engine: Arc<RunEngine>,
        router: Arc<AgentMailboxRouter>,
    }

    impl TestHarness {
        async fn register_parent(&self, run_id: &str) -> AgentMailbox {
            self.router
                .register(AgentAddress::new(run_id, "orch"), None)
                .await
                .unwrap()
        }

        async fn persist_request_parent(&self, request: &DelegationRequest) {
            self.run_engine
                .start_run(
                    &request.parent_run_id,
                    &request.user_id,
                    &request.session_id,
                )
                .await
                .unwrap();
            crate::server::provider_test_support::append_control_plane_contract(
                &self.run_engine,
                &request.user_id,
                &request.session_id,
                &request.parent_run_id,
            )
            .await
            .unwrap();
        }

        async fn execute(
            &self,
            request: DelegationRequest,
            cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
        ) -> Result<astra_services::coordination::DelegationResult, String> {
            self.persist_request_parent(&request).await;
            self.engine.execute(request, "orch", cancel_token).await
        }
    }

    fn setup_harness(executor: Arc<dyn SubRunExecutor>) -> TestHarness {
        let profiles = setup_profiles();
        let store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(store));
        let tracker = Arc::new(DelegationTracker::new());
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker.clone()));

        let engine = DelegationEngine::with_executor(
            profiles,
            run_engine.clone(),
            tracker.clone(),
            executor,
        )
        .with_mailbox_router(router.clone())
        .for_execution(Arc::new(crate::orchestration::DynamicAgentSpawner::new(
            router.clone(),
        )));

        TestHarness {
            engine,
            run_engine,
            router,
        }
    }

    /// Build a harness WITHOUT mailbox_router to test the no-router path.
    fn setup_harness_no_router(executor: Arc<dyn SubRunExecutor>) -> TestHarness {
        let profiles = setup_profiles();
        let store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(store));
        let tracker = Arc::new(DelegationTracker::new());
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker.clone()));

        let engine = DelegationEngine::with_executor(
            profiles,
            run_engine.clone(),
            tracker.clone(),
            executor,
        )
        .for_execution(Arc::new(crate::orchestration::DynamicAgentSpawner::new(
            router.clone(),
        )));
        // Intentionally NOT calling .with_mailbox_router()

        TestHarness {
            engine,
            run_engine,
            router,
        }
    }

    // ── A caller-owned parent is a real message destination ─────────────

    /// Fanout progress reaches the caller's mailbox, not an engine-owned sink.
    #[tokio::test]
    async fn fanout_uses_caller_owned_parent_for_child_progress() {
        let (executor, results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));
        let mut parent = h.register_parent("parent-run").await;
        let request = make_request(
            CoordinationPattern::FanOut {
                agent_ids: vec!["worker-a".into(), "worker-b".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            "parent-run",
            "del-fanout-auto",
        );

        let result = h.execute(request, None).await;
        assert!(result.is_ok(), "delegation should succeed");

        let results = results.lock().await;
        assert_eq!(results.len(), 2);
        for (agent_id, send_result) in results.iter() {
            assert!(
                send_result.is_ok(),
                "agent {agent_id} should reach the caller-owned parent: {send_result:?}"
            );
        }
        assert!(parent.try_recv().is_some());
    }

    /// Ordered children report to the same caller-owned parent.
    #[tokio::test]
    async fn sequential_children_use_caller_owned_parent() {
        let (executor, results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));
        let _parent = h.register_parent("parent-run").await;

        let request = make_request(
            CoordinationPattern::Sequential {
                agent_ids: vec!["team-review-producer".into(), "team-review-reviewer".into()],
                stop_on_success: false,
                timeout_sec: 10,
            },
            "parent-run",
            "del-sequential",
        );

        let result = h.execute(request, None).await;
        assert!(result.is_ok());

        let results = results.lock().await;
        assert!(
            results.len() == 2,
            "both ordered children should report progress"
        );
        for (agent_id, send_result) in results.iter() {
            assert!(
                send_result.is_ok(),
                "agent {agent_id} should succeed sending progress: {send_result:?}"
            );
        }
    }

    /// Sequential children share one caller-owned parent.
    #[tokio::test]
    async fn sequential_uses_caller_owned_parent() {
        let (executor, results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));
        let _parent = h.register_parent("parent-run").await;

        let request = make_request(
            CoordinationPattern::Sequential {
                agent_ids: vec!["worker-a".into(), "worker-b".into()],
                stop_on_success: false,
                timeout_sec: 10,
            },
            "parent-run",
            "del-seq-auto",
        );

        let result = h.execute(request, None).await;
        assert!(result.is_ok());

        let results = results.lock().await;
        for (agent_id, send_result) in results.iter() {
            assert!(
                send_result.is_ok(),
                "sequential agent {agent_id} should succeed: {send_result:?}"
            );
        }
    }

    // ── No consumer must not manufacture successful delivery ────────────

    /// Delegation may finish, but a child cannot queue to a parent with no
    /// actual receiver; the engine must not create a fake mailbox for it.
    #[tokio::test]
    async fn missing_parent_consumer_rejects_child_progress() {
        let (executor, results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));

        let request = make_request(
            CoordinationPattern::FanOut {
                agent_ids: vec!["worker-a".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            "parent-run",
            "del-cleanup",
        );

        let result = h.execute(request, None).await;
        assert!(result.is_ok());
        assert!(results.lock().await.iter().all(|(_, send)| send.is_err()));
        assert!(!h.router.is_run_registered("parent-run").await);
    }

    // ── No-router path: graceful degradation ────────────────────────────

    /// Without a mailbox_router, agents get no mailbox (no panic).
    #[tokio::test]
    async fn no_router_gives_no_mailbox() {
        let (executor, results) = ProgressReportingExecutor::new();
        let h = setup_harness_no_router(Arc::new(executor));

        let request = make_request(
            CoordinationPattern::FanOut {
                agent_ids: vec!["worker-a".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            "parent-run",
            "del-no-router",
        );

        let result = h.execute(request, None).await;
        assert!(result.is_ok());

        let results = results.lock().await;
        assert_eq!(results.len(), 1);
        let (_, send_result) = &results[0];
        assert!(
            send_result.is_err(),
            "without router, agent should have no mailbox"
        );
        assert!(send_result.as_ref().unwrap_err().contains("no mailbox"));
    }

    // ── Unhappy path: parent unregistered mid-execution ─────────────────

    /// Executor that waits for a signal before sending progress.
    struct DelayedProgressExecutor {
        gate: Arc<tokio::sync::Barrier>,
        results: Arc<tokio::sync::Mutex<Vec<(String, Result<(), String>)>>>,
    }

    impl DelayedProgressExecutor {
        fn new(
            agent_count: usize,
        ) -> (
            Self,
            Arc<tokio::sync::Barrier>,
            Arc<tokio::sync::Mutex<Vec<(String, Result<(), String>)>>>,
        ) {
            let barrier = Arc::new(tokio::sync::Barrier::new(agent_count + 1));
            let results = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            (
                Self {
                    gate: barrier.clone(),
                    results: results.clone(),
                },
                barrier,
                results,
            )
        }
    }

    #[async_trait]
    impl SubRunExecutor for DelayedProgressExecutor {
        async fn execute(
            &self,
            config: SubRunConfig,
        ) -> Result<(AgentResult, Option<crate::orchestration::SpawnRunFrontier>), String> {
            let agent_id = config.agent_profile.agent_id.clone();
            let run_id = config.run_id.clone();

            // Wait for test to signal (e.g., after unregistering parent).
            self.gate.wait().await;

            let send_result = if let Some(ref mailbox) = config.mailbox {
                match mailbox.send_progress(0, 1, "turn_complete", None).await {
                    Ok(()) => Ok(()),
                    Err(e) => Err(format!("{e}")),
                }
            } else {
                Err("no mailbox".into())
            };

            self.results
                .lock()
                .await
                .push((agent_id.clone(), send_result));

            Ok((
                AgentResult {
                    agent_id,
                    run_id,
                    status: "completed".into(),
                    output: Some("done".into()),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
                None,
            ))
        }
    }

    /// A terminal parent retired while children are running rejects their
    /// later progress without panicking or hanging. Ordinary turn detach
    /// intentionally keeps the canonical parent route available.
    #[tokio::test]
    async fn parent_terminal_retirement_mid_execution() {
        let (executor, barrier, results) = DelayedProgressExecutor::new(2);
        let h = setup_harness(Arc::new(executor));

        // Keep the original cleanup authority instead of looking up whichever
        // subscription occupies this address after the engine starts.
        let parent_addr = AgentAddress::new("parent-run", "orch");
        let router_clone = h.router.clone();
        let parent_mailbox = router_clone.register(parent_addr, None).await.unwrap();

        let request = make_request(
            CoordinationPattern::FanOut {
                agent_ids: vec!["worker-a".into(), "worker-b".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            "parent-run",
            "del-mid-unreg",
        );
        h.persist_request_parent(&request).await;

        let engine_handle = {
            let engine = h.engine;
            tokio::spawn(async move { engine.execute(request, "orch", None).await })
        };

        // Give the engine time to register parent and spawn agents.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // End the parent lifetime while agents are waiting.
        parent_mailbox.retire().await.unwrap();

        // Release agents — they will now try to send progress.
        barrier.wait().await;

        let result = engine_handle.await.unwrap();
        assert!(result.is_ok(), "delegation should still complete");

        let results = results.lock().await;
        assert_eq!(results.len(), 2);
        for (agent_id, send_result) in results.iter() {
            assert!(
                send_result.is_err(),
                "agent {agent_id} should fail after parent retirement"
            );
        }
    }

    // ── Caller-registered parent coexists with auto-registration ────────

    /// If the caller already registered the parent (e.g., existing
    /// `fanout_agents_send_messages_to_parent` test pattern), the engine's
    /// auto-registration should handle the collision gracefully.
    #[tokio::test]
    async fn caller_pre_registered_parent_still_works() {
        let (executor, results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));

        // Caller registers parent first (old pattern from delegation_mailbox_tests).
        let parent_addr = AgentAddress::new("parent-run", "orch");
        let _parent_mb = h
            .router
            .register(parent_addr, None)
            .await
            .expect("caller register");

        let request = make_request(
            CoordinationPattern::FanOut {
                agent_ids: vec!["worker-a".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            "parent-run",
            "del-pre-reg",
        );

        // Delegation must use this receiver without changing its ownership.
        let result = h.execute(request, None).await;
        assert!(result.is_ok());

        let results = results.lock().await;
        assert_eq!(results.len(), 1);
        let (agent_id, send_result) = &results[0];
        assert!(
            send_result.is_ok(),
            "agent {agent_id} should succeed even with double-registration: {send_result:?}"
        );
    }

    #[tokio::test]
    async fn session_parent_alias_receives_without_unused_turn_mailbox() {
        let (executor, results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));
        let stable = AgentAddress::new("session-root", "orch");
        let mut parent = h.router.register(stable.clone(), None).await.unwrap();
        h.router
            .record_parent_delivery_alias("parent-run", &stable, &stable.agent_id)
            .await;
        let request = make_request(
            CoordinationPattern::FanOut {
                agent_ids: vec!["worker-a".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            "parent-run",
            "del-session-parent",
        );
        assert!(h.execute(request, None).await.is_ok());
        assert!(results.lock().await[0].1.is_ok());
        assert!(!h.router.is_run_registered("parent-run").await);
        assert!(h.router.is_run_registered("session-root").await);
        assert!(
            parent.try_recv().is_some(),
            "the caller-owned consumer receives progress"
        );
    }

    /// Caller registered the same agent_id with a DIFFERENT run_id.
    /// Engine must not clobber the caller's mailbox (run_id mismatch).
    #[tokio::test]
    async fn caller_registered_different_run_id_not_clobbered() {
        let (executor, results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));

        // Caller registers "orch" with a different run_id.
        let caller_addr = AgentAddress::new("caller-run-999", "orch");
        let mut caller_mb = h
            .router
            .register(caller_addr, None)
            .await
            .expect("caller register");

        let request = make_request(
            CoordinationPattern::FanOut {
                agent_ids: vec!["worker-a".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            "parent-run", // different run_id from caller's
            "del-diff-run",
        );

        let result = h.execute(request, None).await;
        assert!(result.is_ok());

        // A different run is not the parent receiver; it cannot make an
        // unbound parent route look deliverable.
        let results = results.lock().await;
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_err());

        // Caller's mailbox should NOT have been clobbered — it should still
        // be functional (no messages expected, but not disconnected).
        assert!(
            caller_mb.try_recv().is_none(),
            "caller mailbox should be intact (no messages, not broken)"
        );
    }

    // ── Fork pattern ────────────────────────────────────────────────────

    #[tokio::test]
    async fn fork_uses_caller_owned_parent() {
        let (executor, results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));
        let _parent = h.register_parent("parent-run").await;

        let request = make_request(
            CoordinationPattern::Fork {
                agent_id: "worker-a".into(),
                tasks: vec!["task-1".into(), "task-2".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            "parent-run",
            "del-fork-auto",
        );

        let result = h.execute(request, None).await;
        assert!(result.is_ok());

        let results = results.lock().await;
        assert_eq!(results.len(), 2, "fork should spawn 2 sub-runs");
        for (agent_id, send_result) in results.iter() {
            assert!(
                send_result.is_ok(),
                "fork agent {agent_id} should succeed: {send_result:?}"
            );
        }
    }

    // ── Drop safety: child mailboxes auto-unregister ────────────────────

    /// After delegation completes and child mailboxes are dropped, the
    /// Drop impl should unregister them from the router.
    #[tokio::test]
    async fn child_mailboxes_cleaned_up_via_drop() {
        let (executor, _results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));

        let request = make_request(
            CoordinationPattern::FanOut {
                agent_ids: vec!["worker-a".into(), "worker-b".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            "parent-run",
            "del-drop-cleanup",
        );

        let result = h.execute(request, None).await;
        assert!(result.is_ok());

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let registered = h
                    .router
                    .list_registered_agents("del-drop-cleanup")
                    .await
                    .unwrap();
                if registered.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all parent and child mailboxes should unregister after their handles drop");
    }

    // ── Cancellation: fan-out collection loop aborts promptly ───────────

    /// Executor that blocks forever (ignores cancel_token).
    /// Tests that the collection loop itself handles cancellation.
    struct UncooperativeExecutor;

    #[async_trait]
    impl SubRunExecutor for UncooperativeExecutor {
        async fn execute(
            &self,
            config: SubRunConfig,
        ) -> Result<(AgentResult, Option<crate::orchestration::SpawnRunFrontier>), String> {
            let agent_id = config.agent_profile.agent_id.clone();
            let run_id = config.run_id.clone();
            // Ignore cancel_token — block on a channel that never sends.
            let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
            let _ = rx.await;
            Ok((
                AgentResult {
                    agent_id,
                    run_id,
                    status: "completed".into(),
                    output: None,
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
                None,
            ))
        }
    }

    /// Fan-out with uncooperative executor: cancel token fires, collection
    /// loop should abort tasks and return within a bounded time.
    #[tokio::test]
    async fn fanout_collection_loop_aborts_on_cancel() {
        let h = setup_harness(Arc::new(UncooperativeExecutor));

        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let request = make_request(
            CoordinationPattern::FanOut {
                agent_ids: vec!["worker-a".into(), "worker-b".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 0,
            },
            "parent-run",
            "del-abort-fanout",
        );
        h.persist_request_parent(&request).await;

        let cancel_clone = cancel.clone();
        let engine_handle = {
            let engine = h.engine;
            tokio::spawn(async move { engine.execute(request, "orch", Some(cancel_clone)).await })
        };

        // Let agents start.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancel.cancel();

        // The collection loop should abort tasks and return promptly.
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), engine_handle).await;

        let delegation = result
            .expect("fan-out should complete within 2s after cancel")
            .expect("fan-out task should join")
            .expect("fan-out cancellation should return a terminal delegation result");
        assert_eq!(delegation.agent_results.len(), 2);
        assert!(
            delegation
                .agent_results
                .iter()
                .all(|result| result.status == astra_core::STATUS_CANCELLED),
            "every force-aborted child must be represented as cancelled: {:?}",
            delegation.agent_results
        );
    }

    /// Fork with uncooperative executor: same test for fork pattern.
    #[tokio::test]
    async fn fork_collection_loop_aborts_on_cancel() {
        let h = setup_harness(Arc::new(UncooperativeExecutor));

        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let request = make_request(
            CoordinationPattern::Fork {
                agent_id: "worker-a".into(),
                tasks: vec!["task-1".into(), "task-2".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 0,
            },
            "parent-run",
            "del-abort-fork",
        );
        h.persist_request_parent(&request).await;

        let cancel_clone = cancel.clone();
        let engine_handle = {
            let engine = h.engine;
            tokio::spawn(async move { engine.execute(request, "orch", Some(cancel_clone)).await })
        };

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancel.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), engine_handle).await;

        let delegation = result
            .expect("fork should complete within 2s after cancel")
            .expect("fork task should join")
            .expect("fork cancellation should return a terminal delegation result");
        assert_eq!(delegation.agent_results.len(), 2);
        assert!(
            delegation
                .agent_results
                .iter()
                .all(|result| result.status == astra_core::STATUS_CANCELLED),
            "every force-aborted fork child must be represented as cancelled: {:?}",
            delegation.agent_results
        );
    }
}
