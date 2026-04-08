//! Integration test: DelegationEngine + Mailbox end-to-end.
//!
//! Verifies that when a DelegationEngine has a mailbox_router wired,
//! spawned sub-run agents receive functional mailboxes and can exchange
//! messages with each other and the parent.

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
    use crate::messaging::router::{AgentMailbox, AgentMailboxRouter};
    use crate::messaging::types::*;
    use crate::server::delegation_engine::{
        DelegationEngine, DelegationTracker, SubRunConfig, SubRunExecutor,
    };
    use crate::server::run_engine::RunEngine;

    // ── Custom executor that uses mailbox to communicate ────────────────────

    /// An executor where each "agent" sends a message to parent via mailbox,
    /// then reads any peer broadcast messages, and returns output describing
    /// what it saw.
    struct MailboxTestExecutor;

    #[async_trait]
    impl SubRunExecutor for MailboxTestExecutor {
        async fn execute(&self, mut config: SubRunConfig) -> Result<AgentResult, String> {
            let agent_id = config.agent_profile.agent_id.clone();
            let run_id = config.run_id.clone();

            if let Some(ref mut mailbox) = config.mailbox {
                // 1. Send a "hello" message to parent.
                let hello_msg = AgentMessage::new(
                    mailbox.address.clone(),
                    MessageTarget::Parent,
                    MessagePayload::Text {
                        content: format!("Hello from {agent_id}"),
                        summary: None,
                    },
                );
                let _ = mailbox.send(hello_msg).await;

                // 2. Broadcast to all peers in the delegation group.
                let did = mailbox.delegation_id.clone().unwrap_or_default();
                let broadcast_msg = AgentMessage::new(
                    mailbox.address.clone(),
                    MessageTarget::Broadcast {
                        delegation_id: did,
                    },
                    MessagePayload::Text {
                        content: format!("{agent_id} reporting in"),
                        summary: None,
                    },
                );
                let _ = mailbox.send(broadcast_msg).await;

                // 3. Small delay to allow broadcast propagation within InProcessTransport.
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;

                // 4. Drain any received messages.
                let mut received = Vec::new();
                while let Some(msg) = mailbox.try_recv() {
                    if let MessagePayload::Text { content, .. } = &msg.payload {
                        received.push(content.clone());
                    }
                }

                // 5. Send progress to parent.
                let _ = mailbox
                    .send_progress(0, 0, "completed", Some(format!("{agent_id} done")))
                    .await;

                Ok(AgentResult {
                    agent_id,
                    run_id,
                    status: "completed".to_string(),
                    output: Some(format!(
                        "mailbox=true, sent=2, received={}",
                        received.len()
                    )),
                    error: None,
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    tool_calls: 0,
                })
            } else {
                Ok(AgentResult {
                    agent_id,
                    run_id,
                    status: "completed".to_string(),
                    output: Some("mailbox=false".to_string()),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            }
        }
    }

    // ── Setup helpers ───────────────────────────────────────────────────────

    fn setup_profiles() -> Arc<RwLock<AgentProfileRegistry>> {
        let mut reg = AgentProfileRegistry::new();
        reg.register(AgentProfile::new("orch", "Orchestrator", AgentTier::Orchestrator))
            .unwrap();
        reg.register(AgentProfile::new("coder", "Coder", AgentTier::System))
            .unwrap();
        reg.register(AgentProfile::new("reviewer", "Reviewer", AgentTier::System))
            .unwrap();
        reg.register(AgentProfile::new("tester", "Tester", AgentTier::System))
            .unwrap();
        Arc::new(RwLock::new(reg))
    }

    fn fan_out_request(agents: Vec<&str>) -> DelegationRequest {
        DelegationRequest {
            delegation_id: "del-mailbox-test".into(),
            parent_run_id: "parent-run".into(),
            task: "test task with mailbox".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: agents.into_iter().map(String::from).collect(),
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 30,
            },
            user_id: "user-1".into(),
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

    // ── Tests ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fanout_agents_receive_functional_mailboxes() {
        let profiles = setup_profiles();
        let store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(store));
        let tracker = Arc::new(DelegationTracker::new());
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker.clone()));

        let engine = DelegationEngine::with_executor(
            profiles,
            run_engine,
            tracker.clone(),
            Arc::new(MailboxTestExecutor),
        )
        .with_mailbox_router(router.clone());

        let request = fan_out_request(vec!["coder", "reviewer"]);
        let result = engine.execute(request, "orch").await;

        assert!(result.is_ok(), "delegation should succeed: {result:?}");
        let delegation_result = result.unwrap();

        // All agents should have completed with mailbox=true.
        for agent_result in &delegation_result.agent_results {
            let output = agent_result.output.as_deref().unwrap_or("");
            assert!(
                output.starts_with("mailbox=true"),
                "agent {} should have received a mailbox, got: {output}",
                agent_result.agent_id
            );
        }
    }

    #[tokio::test]
    async fn fanout_agents_send_messages_to_parent() {
        let profiles = setup_profiles();
        let store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(store));
        let tracker = Arc::new(DelegationTracker::new());
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker.clone()));

        // Register the parent so it can receive messages.
        let parent_addr = AgentAddress::new("parent-run", "orch");
        let mut parent_mb = router
            .register(parent_addr, None)
            .await
            .expect("parent register");

        let engine = DelegationEngine::with_executor(
            profiles,
            run_engine,
            tracker.clone(),
            Arc::new(MailboxTestExecutor),
        )
        .with_mailbox_router(router.clone());

        let request = fan_out_request(vec!["coder", "reviewer"]);
        let result = engine.execute(request, "orch").await;
        assert!(result.is_ok());

        // Parent should have received messages from both agents.
        // Each agent sends: 1 hello (Text) + 1 progress (Progress) = 2 per agent.
        let mut parent_msgs = Vec::new();
        while let Some(msg) = parent_mb.try_recv() {
            parent_msgs.push(msg);
        }

        // At minimum we should get the "Hello from ..." text messages.
        let hello_msgs: Vec<_> = parent_msgs
            .iter()
            .filter(|m| matches!(&m.payload, MessagePayload::Text { content, .. } if content.starts_with("Hello from")))
            .collect();

        assert!(
            hello_msgs.len() >= 2,
            "parent should receive hello from both agents, got {} messages: {:?}",
            hello_msgs.len(),
            hello_msgs.iter().map(|m| &m.payload).collect::<Vec<_>>()
        );

        // Verify senders.
        let senders: std::collections::HashSet<_> =
            hello_msgs.iter().map(|m| m.from.agent_id.as_str()).collect();
        assert!(senders.contains("coder"), "missing coder hello");
        assert!(senders.contains("reviewer"), "missing reviewer hello");

        // Progress messages should also be present.
        let progress_msgs: Vec<_> = parent_msgs
            .iter()
            .filter(|m| matches!(&m.payload, MessagePayload::Progress { .. }))
            .collect();
        assert!(
            progress_msgs.len() >= 2,
            "parent should receive progress from both agents, got {}",
            progress_msgs.len()
        );
    }

    #[tokio::test]
    async fn fanout_agents_receive_peer_broadcasts() {
        let profiles = setup_profiles();
        let store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(store));
        let tracker = Arc::new(DelegationTracker::new());
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker.clone()));

        let engine = DelegationEngine::with_executor(
            profiles,
            run_engine,
            tracker.clone(),
            Arc::new(MailboxTestExecutor),
        )
        .with_mailbox_router(router.clone());

        let request = fan_out_request(vec!["coder", "reviewer", "tester"]);
        let result = engine.execute(request, "orch").await;
        assert!(result.is_ok());

        let delegation_result = result.unwrap();

        // With 3 agents each broadcasting, each should receive messages from peers.
        // The exact count depends on timing (broadcasts are async), but at least
        // some agents should have received > 0 peer messages.
        let mut any_received_peers = false;
        for agent_result in &delegation_result.agent_results {
            let output = agent_result.output.as_deref().unwrap_or("");
            if let Some(received) = output
                .split("received=")
                .nth(1)
                .and_then(|s| s.parse::<usize>().ok())
            {
                if received > 0 {
                    any_received_peers = true;
                }
            }
        }

        assert!(
            any_received_peers,
            "at least one agent should have received peer broadcasts, results: {:?}",
            delegation_result
                .agent_results
                .iter()
                .map(|r| format!("{}: {}", r.agent_id, r.output.as_deref().unwrap_or("")))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn fanout_without_router_gives_no_mailbox() {
        let profiles = setup_profiles();
        let store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(store));
        let tracker = Arc::new(DelegationTracker::new());

        // No mailbox_router → agents should get mailbox=None.
        let engine = DelegationEngine::with_executor(
            profiles,
            run_engine,
            tracker,
            Arc::new(MailboxTestExecutor),
        );
        // Intentionally NOT calling .with_mailbox_router()

        let request = fan_out_request(vec!["coder"]);
        let result = engine.execute(request, "orch").await;
        assert!(result.is_ok());

        let delegation_result = result.unwrap();
        assert_eq!(delegation_result.agent_results.len(), 1);
        assert_eq!(
            delegation_result.agent_results[0]
                .output
                .as_deref()
                .unwrap_or(""),
            "mailbox=false"
        );
    }

    #[tokio::test]
    async fn delegation_tracker_records_sub_runs_for_parent_resolution() {
        let profiles = setup_profiles();
        let store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(store));
        let tracker = Arc::new(DelegationTracker::new());
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker.clone()));

        let engine = DelegationEngine::with_executor(
            profiles,
            run_engine,
            tracker.clone(),
            Arc::new(MailboxTestExecutor),
        )
        .with_mailbox_router(router);

        let request = fan_out_request(vec!["coder", "reviewer"]);
        let _ = engine.execute(request, "orch").await;

        // DelegationTracker should have records for both sub-runs.
        let sub_runs = tracker.get_sub_runs("del-mailbox-test").await;
        assert_eq!(sub_runs.len(), 2, "should have 2 sub-run records");

        // Parent resolution should work.
        for sub in &sub_runs {
            let parent = tracker.get_parent(&sub.run_id).await;
            assert_eq!(
                parent.as_deref(),
                Some("parent-run"),
                "sub-run {} should have parent-run as parent",
                sub.run_id
            );
        }
    }
}
