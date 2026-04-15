//! Tests for orchestrator mailbox registration in team delegations.
//!
//! Reproduces the bug where `/team run` child agents fail to send progress
//! to the parent orchestrator because the orchestrator never registers its
//! own mailbox with the router.
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

    use crate::messaging::in_process::InProcessTransport;
    use crate::messaging::router::AgentMailboxRouter;
    use crate::messaging::types::*;
    use crate::server::delegation_engine::{
        DelegationEngine, DelegationTracker, SubRunConfig, SubRunExecutor,
    };
    use crate::server::run_engine::RunEngine;

    // ── Executor that records whether send_progress succeeds ────────────

    struct ProgressReportingExecutor {
        results: Arc<tokio::sync::Mutex<Vec<(String, Result<(), String>)>>>,
    }

    impl ProgressReportingExecutor {
        fn new() -> (Self, Arc<tokio::sync::Mutex<Vec<(String, Result<(), String>)>>>) {
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
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
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

            Ok(AgentResult {
                agent_id,
                run_id,
                status: "completed".into(),
                output: Some("done".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────────

    fn setup_profiles() -> Arc<RwLock<AgentProfileRegistry>> {
        let mut reg = AgentProfileRegistry::new();
        reg.register(AgentProfile::new("orch", "Orchestrator", AgentTier::Orchestrator))
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
            delegation_id: delegation_id.into(),
            parent_run_id: parent_run_id.into(),
            task: "test task".into(),
            pattern,
            user_id: "test-user".into(),
            depth: 0,
            context: {
                let mut ctx = HashMap::new();
                ctx.insert(
                    "session_id".into(),
                    serde_json::Value::String("test-session".into()),
                );
                ctx
            },
        }
    }

    struct TestHarness {
        engine: DelegationEngine,
        router: Arc<AgentMailboxRouter>,
        #[allow(dead_code)]
        tracker: Arc<DelegationTracker>,
    }

    fn setup_harness(executor: Arc<dyn SubRunExecutor>) -> TestHarness {
        let profiles = setup_profiles();
        let store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(store));
        let tracker = Arc::new(DelegationTracker::new());
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker.clone()));

        let engine =
            DelegationEngine::with_executor(profiles, run_engine, tracker.clone(), executor)
                .with_mailbox_router(router.clone());

        TestHarness {
            engine,
            router,
            tracker,
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

        let engine =
            DelegationEngine::with_executor(profiles, run_engine, tracker.clone(), executor);
        // Intentionally NOT calling .with_mailbox_router()

        TestHarness {
            engine,
            router,
            tracker,
        }
    }

    // ── Core fix: engine auto-registers parent ──────────────────────────

    /// DelegationEngine auto-registers the parent mailbox so child agents
    /// can send progress without the caller needing to register manually.
    #[tokio::test]
    async fn fanout_auto_registers_parent_for_child_progress() {
        let (executor, results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));

        // Do NOT manually register the parent — the engine should do it.
        let request = make_request(
            CoordinationPattern::FanOut {
                agent_ids: vec!["worker-a".into(), "worker-b".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            "parent-run",
            "del-fanout-auto",
        );

        let result = h.engine.execute(request, "orch", None).await;
        assert!(result.is_ok(), "delegation should succeed");

        let results = results.lock().await;
        assert_eq!(results.len(), 2);
        for (agent_id, send_result) in results.iter() {
            assert!(
                send_result.is_ok(),
                "agent {agent_id} should succeed sending progress (engine auto-registered parent): {send_result:?}"
            );
        }
    }

    /// Adversarial review also auto-registers parent across multiple rounds.
    #[tokio::test]
    async fn adversarial_auto_registers_parent_for_child_progress() {
        let (executor, results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));

        let request = make_request(
            CoordinationPattern::AdversarialReview {
                producer_id: "team-review-producer".into(),
                reviewer_id: "team-review-reviewer".into(),
                max_rounds: 2,
                timeout_sec: 10,
                acceptance_threshold: 0.8,
            },
            "parent-run",
            "del-adv-auto",
        );

        let result = h.engine.execute(request, "orch", None).await;
        assert!(result.is_ok());

        let results = results.lock().await;
        assert!(
            results.len() >= 2,
            "should have at least producer + reviewer results"
        );
        for (agent_id, send_result) in results.iter() {
            assert!(
                send_result.is_ok(),
                "agent {agent_id} should succeed sending progress: {send_result:?}"
            );
        }
    }

    /// Sequential pattern also auto-registers parent.
    #[tokio::test]
    async fn sequential_auto_registers_parent() {
        let (executor, results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));

        let request = make_request(
            CoordinationPattern::Sequential {
                agent_ids: vec!["worker-a".into(), "worker-b".into()],
                stop_on_success: false,
                timeout_sec: 10,
            },
            "parent-run",
            "del-seq-auto",
        );

        let result = h.engine.execute(request, "orch", None).await;
        assert!(result.is_ok());

        let results = results.lock().await;
        for (agent_id, send_result) in results.iter() {
            assert!(
                send_result.is_ok(),
                "sequential agent {agent_id} should succeed: {send_result:?}"
            );
        }
    }

    // ── Cleanup: parent unregistered after delegation ───────────────────

    /// Auto-registered parent mailbox is cleaned up after delegation completes.
    #[tokio::test]
    async fn auto_registered_parent_cleaned_up_after_delegation() {
        let (executor, _results) = ProgressReportingExecutor::new();
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

        let result = h.engine.execute(request, "orch", None).await;
        assert!(result.is_ok());

        // After delegation completes, the auto-registered parent should be
        // unregistered so it doesn't leak resources or collide with future runs.
        let registered = h.router.list_registered_agents().await;
        assert!(
            !registered.contains(&"orch".to_string()),
            "parent should be unregistered after delegation, still registered: {registered:?}"
        );
    }

    /// Children are also unregistered after delegation (no leaked mailboxes).
    /// Note: child mailbox cleanup depends on the executor dropping the
    /// SubRunConfig. The engine only guarantees parent cleanup.
    #[tokio::test]
    async fn parent_cleaned_up_even_when_children_linger() {
        let (executor, _results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));

        let request = make_request(
            CoordinationPattern::FanOut {
                agent_ids: vec!["worker-a".into(), "worker-b".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            "parent-run",
            "del-all-cleanup",
        );

        let result = h.engine.execute(request, "orch", None).await;
        assert!(result.is_ok());

        let registered = h.router.list_registered_agents().await;
        // Parent ("orch") must be cleaned up by the engine.
        assert!(
            !registered.contains(&"orch".to_string()),
            "parent should be unregistered after delegation, still registered: {registered:?}"
        );
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

        let result = h.engine.execute(request, "orch", None).await;
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
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let agent_id = config.agent_profile.agent_id.clone();
            let run_id = config.run_id.clone();

            // Wait for test to signal (e.g., after unregistering parent).
            self.gate.wait().await;

            let send_result = if let Some(ref mailbox) = config.mailbox {
                match mailbox
                    .send_progress(0, 1, "turn_complete", None)
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

            Ok(AgentResult {
                agent_id,
                run_id,
                status: "completed".into(),
                output: Some("done".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    /// Parent forcibly unregistered while children are running → children
    /// get AgentNotFound (not panic or hang). This simulates the race where
    /// an external caller unregisters the parent (e.g., session cleanup).
    #[tokio::test]
    async fn parent_forcibly_unregistered_mid_execution() {
        let (executor, barrier, results) = DelayedProgressExecutor::new(2);
        let h = setup_harness(Arc::new(executor));

        // The engine will auto-register the parent. We need to unregister it
        // after the engine starts but before agents send progress.
        let parent_addr = AgentAddress::new("parent-run", "orch");
        let router_clone = h.router.clone();

        let request = make_request(
            CoordinationPattern::FanOut {
                agent_ids: vec!["worker-a".into(), "worker-b".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            "parent-run",
            "del-mid-unreg",
        );

        let engine_handle = {
            let engine = h.engine;
            tokio::spawn(async move { engine.execute(request, "orch", None).await })
        };

        // Give the engine time to register parent and spawn agents.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Forcibly unregister the parent while agents are waiting.
        let _ = router_clone.unregister(&parent_addr).await;

        // Release agents — they will now try to send progress.
        barrier.wait().await;

        let result = engine_handle.await.unwrap();
        assert!(result.is_ok(), "delegation should still complete");

        let results = results.lock().await;
        assert_eq!(results.len(), 2);
        for (agent_id, send_result) in results.iter() {
            assert!(
                send_result.is_err(),
                "agent {agent_id} should fail after parent forcibly unregistered"
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

        // Engine will attempt to re-register the same parent — should not panic.
        let result = h.engine.execute(request, "orch", None).await;
        assert!(result.is_ok());

        let results = results.lock().await;
        assert_eq!(results.len(), 1);
        let (agent_id, send_result) = &results[0];
        assert!(
            send_result.is_ok(),
            "agent {agent_id} should succeed even with double-registration: {send_result:?}"
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

        let result = h.engine.execute(request, "orch", None).await;
        assert!(result.is_ok());

        // Child progress should still succeed (engine registered parent-run).
        let results = results.lock().await;
        assert_eq!(results.len(), 1);
        assert!(
            results[0].1.is_ok(),
            "child should succeed: {:?}",
            results[0].1
        );

        // Caller's mailbox should NOT have been clobbered — it should still
        // be functional (no messages expected, but not disconnected).
        assert!(
            caller_mb.try_recv().is_none(),
            "caller mailbox should be intact (no messages, not broken)"
        );
    }

    // ── Cancellation: parent cleaned up even on cancel ──────────────────

    struct BlockingExecutor;

    #[async_trait]
    impl SubRunExecutor for BlockingExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let agent_id = config.agent_profile.agent_id.clone();
            let run_id = config.run_id.clone();

            if let Some(ref token) = config.cancel_token {
                token.cancelled().await;
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }

            Ok(AgentResult {
                agent_id,
                run_id,
                status: "completed".into(),
                output: Some("cancelled".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    /// When delegation is cancelled, parent mailbox is still cleaned up.
    #[tokio::test]
    async fn parent_cleaned_up_after_cancellation() {
        let h = setup_harness(Arc::new(BlockingExecutor));

        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let request = make_request(
            CoordinationPattern::FanOut {
                agent_ids: vec!["worker-a".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 0, // no timeout, rely on cancel
            },
            "parent-run",
            "del-cancel",
        );

        let cancel_clone = cancel.clone();
        let engine_handle = {
            let engine = h.engine;
            tokio::spawn(
                async move { engine.execute(request, "orch", Some(cancel_clone)).await },
            )
        };

        // Give engine time to register parent and spawn agents.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Verify parent is registered.
        assert!(
            h.router.is_run_registered("parent-run").await,
            "parent should be registered before cancel"
        );

        // Cancel.
        cancel.cancel();
        let _ = engine_handle.await;

        // Parent should be cleaned up even after cancellation.
        assert!(
            !h.router.is_run_registered("parent-run").await,
            "parent should be unregistered after cancellation"
        );
    }

    // ── Fork pattern ────────────────────────────────────────────────────

    #[tokio::test]
    async fn fork_auto_registers_parent() {
        let (executor, results) = ProgressReportingExecutor::new();
        let h = setup_harness(Arc::new(executor));

        let request = make_request(
            CoordinationPattern::Fork {
                agent_id: "worker-a".into(),
                tasks: vec!["task-1".into(), "task-2".into()],
                max_turns: 10,
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 10,
            },
            "parent-run",
            "del-fork-auto",
        );

        let result = h.engine.execute(request, "orch", None).await;
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

        let result = h.engine.execute(request, "orch", None).await;
        assert!(result.is_ok());

        // Give the Drop-spawned unregister tasks time to complete.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let registered = h.router.list_registered_agents().await;
        assert!(
            registered.is_empty(),
            "all mailboxes (parent + children) should be cleaned up via Drop, still registered: {registered:?}"
        );
    }

    // ── Cancellation: fan-out collection loop aborts promptly ───────────

    /// Executor that blocks forever (ignores cancel_token).
    /// Tests that the collection loop itself handles cancellation.
    struct UncooperativeExecutor;

    #[async_trait]
    impl SubRunExecutor for UncooperativeExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let agent_id = config.agent_profile.agent_id.clone();
            let run_id = config.run_id.clone();
            // Ignore cancel_token — block on a channel that never sends.
            let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
            let _ = rx.await;
            Ok(AgentResult {
                agent_id,
                run_id,
                status: "completed".into(),
                output: None,
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
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

        let cancel_clone = cancel.clone();
        let engine_handle = {
            let engine = h.engine;
            tokio::spawn(
                async move { engine.execute(request, "orch", Some(cancel_clone)).await },
            )
        };

        // Let agents start.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancel.cancel();

        // The collection loop should abort tasks and return promptly.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            engine_handle,
        )
        .await;

        assert!(
            result.is_ok(),
            "fan-out should complete within 2s after cancel (not block forever)"
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
                max_turns: 10,
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 0,
            },
            "parent-run",
            "del-abort-fork",
        );

        let cancel_clone = cancel.clone();
        let engine_handle = {
            let engine = h.engine;
            tokio::spawn(
                async move { engine.execute(request, "orch", Some(cancel_clone)).await },
            )
        };

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancel.cancel();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            engine_handle,
        )
        .await;

        assert!(
            result.is_ok(),
            "fork should complete within 2s after cancel (not block forever)"
        );
    }

    // ── register_if_absent: no clobber on concurrent registration ───────

    /// Verify register_if_absent returns None when run_id already registered.
    #[tokio::test]
    async fn register_if_absent_skips_existing() {
        let _profiles = setup_profiles();
        let store = Arc::new(InMemoryRunStateStore::new());
        let _run_engine = Arc::new(RunEngine::new(store));
        let tracker = Arc::new(DelegationTracker::new());
        let transport = Arc::new(crate::messaging::in_process::InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker));

        // First registration succeeds.
        let addr = AgentAddress::new("run-1", "agent-1");
        let result = router.register_if_absent(addr.clone(), None).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_some(), "first registration should return Some");

        // Second registration with same run_id returns None (no clobber).
        let addr2 = AgentAddress::new("run-1", "agent-1");
        let result2 = router.register_if_absent(addr2, None).await;
        assert!(result2.is_ok());
        assert!(result2.unwrap().is_none(), "second registration should return None");
    }

    /// register_if_absent with different run_id registers both.
    #[tokio::test]
    async fn register_if_absent_allows_different_run_ids() {
        let _profiles = setup_profiles();
        let store = Arc::new(InMemoryRunStateStore::new());
        let _run_engine = Arc::new(RunEngine::new(store));
        let tracker = Arc::new(DelegationTracker::new());
        let transport = Arc::new(crate::messaging::in_process::InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker));

        let addr1 = AgentAddress::new("run-1", "agent-1");
        let r1 = router.register_if_absent(addr1, None).await.unwrap();
        assert!(r1.is_some());

        let addr2 = AgentAddress::new("run-2", "agent-1");
        let r2 = router.register_if_absent(addr2, None).await.unwrap();
        assert!(r2.is_some(), "different run_id should register successfully");
    }
}
