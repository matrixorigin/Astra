//! Integration tests for inter-agent messaging.
//!
//! These tests verify end-to-end messaging flows that span multiple components:
//! router + transport + delegation tracker + send_tool.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use astra_messaging::in_process::InProcessTransport;
    use astra_messaging::router::{AgentMailbox, AgentMailboxRouter};
    use astra_messaging::send_tool;
    use astra_messaging::types::*;
    use crate::server::delegation_engine::{DelegationTracker, SubRunRecord, SubRunState};

    fn tracker() -> Arc<DelegationTracker> {
        Arc::new(DelegationTracker::new())
    }

    fn addr(run: &str, agent: &str) -> AgentAddress {
        AgentAddress::new(run, agent)
    }

    /// Helper: set up a delegation with N child agents under one parent.
    async fn setup_delegation(
        n_children: usize,
        delegation_id: &str,
    ) -> (
        Arc<AgentMailboxRouter>,
        AgentMailbox,
        Vec<AgentMailbox>,
        Arc<DelegationTracker>,
    ) {
        let transport = Arc::new(InProcessTransport::new());
        let dt = tracker();
        let router = Arc::new(AgentMailboxRouter::new(transport, dt.clone()));

        // Register parent (orchestrator)
        let parent_addr = addr("run-parent", "orchestrator");
        let parent_mb = router.register(parent_addr.clone(), None).await.unwrap();

        let mut children = Vec::new();
        for i in 0..n_children {
            let child_id = format!("agent-{i}");
            let child_run = format!("run-child-{i}");
            let child_addr = addr(&child_run, &child_id);

            // Record parent→child relationship
            dt.record_sub_run(SubRunRecord {
                run_id: child_run.clone(),
                parent_run_id: "run-parent".into(),
                delegation_id: delegation_id.into(),
                agent_id: child_id.clone(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

            let mb = router
                .register(child_addr, Some(delegation_id.into()))
                .await
                .unwrap();
            children.push(mb);
        }

        (router, parent_mb, children, dt)
    }

    // ─── FanOut multi-agent communication ────────────────────────────────────

    #[tokio::test]
    async fn fanout_agents_can_send_to_each_other() {
        let (_router, _parent, mut children, _dt) = setup_delegation(3, "del-fanout").await;

        // Agent-0 sends a direct message to Agent-1
        let msg = AgentMessage::new(
            children[0].address.clone(),
            MessageTarget::Direct {
                address: children[1].address.clone(),
            },
            MessagePayload::Text {
                content: "I found a bug in auth.rs".into(),
                summary: None,
            },
        );
        children[0].send(msg).await.unwrap();

        // Agent-1 receives it
        let received = children[1].try_recv().unwrap();
        assert_eq!(received.from.agent_id, "agent-0");
        match &received.payload {
            MessagePayload::Text { content, .. } => {
                assert_eq!(content, "I found a bug in auth.rs");
            }
            _ => panic!("expected Text payload"),
        }

        // Agent-2 did NOT receive it (direct, not broadcast)
        assert!(children[2].try_recv().is_none());
    }

    #[tokio::test]
    async fn fanout_agents_broadcast_to_peers() {
        let (_router, _parent, mut children, _dt) = setup_delegation(3, "del-broadcast").await;

        // Agent-0 broadcasts to all peers in the delegation group
        let msg = AgentMessage::new(
            children[0].address.clone(),
            MessageTarget::Broadcast {
                delegation_id: "del-broadcast".into(),
            },
            MessagePayload::Text {
                content: "sync point reached".into(),
                summary: None,
            },
        );
        children[0].send(msg).await.unwrap();

        // All 3 agents receive the broadcast (including sender)
        for (i, child) in children.iter_mut().enumerate() {
            let received = child.try_recv();
            assert!(
                received.is_some(),
                "agent-{i} should have received broadcast"
            );
            match &received.unwrap().payload {
                MessagePayload::Text { content, .. } => {
                    assert_eq!(content, "sync point reached");
                }
                _ => panic!("expected Text payload for agent-{i}"),
            }
        }
    }

    #[tokio::test]
    async fn broadcast_isolation_between_delegations() {
        let transport = Arc::new(InProcessTransport::new());
        let dt = tracker();
        let router = Arc::new(AgentMailboxRouter::new(transport, dt.clone()));

        // Delegation A: two agents
        let a1 = addr("run-a1", "coder-a");
        let a2 = addr("run-a2", "reviewer-a");
        let _mb_a1 = router
            .register(a1.clone(), Some("del-A".into()))
            .await
            .unwrap();
        let mut mb_a2 = router
            .register(a2.clone(), Some("del-A".into()))
            .await
            .unwrap();

        // Delegation B: one agent
        let b1 = addr("run-b1", "coder-b");
        let mut mb_b1 = router
            .register(b1.clone(), Some("del-B".into()))
            .await
            .unwrap();

        // Broadcast to delegation A
        let msg = AgentMessage::new(
            a1.clone(),
            MessageTarget::Broadcast {
                delegation_id: "del-A".into(),
            },
            MessagePayload::Text {
                content: "A-only message".into(),
                summary: None,
            },
        );
        router.send(msg).await.unwrap();

        // Agent in delegation A receives it
        assert!(mb_a2.try_recv().is_some());
        // Agent in delegation B does NOT
        assert!(mb_b1.try_recv().is_none());
    }

    // ─── Parent communication ───────────────────────────────────────────────

    #[tokio::test]
    async fn child_sends_to_parent() {
        let (_router, mut parent, children, _dt) = setup_delegation(2, "del-parent").await;

        // Child-0 sends a message to parent
        children[0].send_to_parent("task complete").await.unwrap();

        let received = parent.try_recv().unwrap();
        assert_eq!(received.from.agent_id, "agent-0");
        match &received.payload {
            MessagePayload::Text { content, .. } => {
                assert_eq!(content, "task complete");
            }
            _ => panic!("expected Text"),
        }
    }

    #[tokio::test]
    async fn child_sends_progress_to_parent() {
        let (_router, mut parent, children, _dt) = setup_delegation(1, "del-progress").await;

        children[0]
            .send_progress(3, 7, "running", Some("executing bash".into()))
            .await
            .unwrap();

        let received = parent.try_recv().unwrap();
        match &received.payload {
            MessagePayload::Progress {
                turn_index,
                tool_calls,
                status,
                detail,
            } => {
                assert_eq!(*turn_index, 3);
                assert_eq!(*tool_calls, 7);
                assert_eq!(status, "running");
                assert_eq!(detail.as_deref(), Some("executing bash"));
            }
            _ => panic!("expected Progress"),
        }
    }

    #[tokio::test]
    async fn parent_sends_to_child() {
        let (_router, parent, mut children, _dt) = setup_delegation(2, "del-parent-to-child").await;

        // Parent sends directly to child-1
        let msg = AgentMessage::new(
            parent.address.clone(),
            MessageTarget::Direct {
                address: children[1].address.clone(),
            },
            MessagePayload::Signal(AgentSignal::Idle),
        );
        parent.send(msg).await.unwrap();

        let received = children[1].try_recv().unwrap();
        assert_eq!(received.from.agent_id, "orchestrator");
        assert!(matches!(
            received.payload,
            MessagePayload::Signal(AgentSignal::Idle)
        ));
        // Child-0 didn't get it
        assert!(children[0].try_recv().is_none());
    }

    // ─── send_message tool end-to-end ───────────────────────────────────────

    #[tokio::test]
    async fn send_tool_text_to_parent() {
        let (_router, mut parent, children, _dt) = setup_delegation(1, "del-tool").await;

        let args = serde_json::json!({
            "target": "parent",
            "content": "I finished the refactoring"
        });
        let result = send_tool::execute_send_message(&children[0], &args).await;
        assert!(
            result.display.starts_with("✓"),
            "Expected success, got: {}",
            result.display
        );

        let received = parent.try_recv().unwrap();
        match &received.payload {
            MessagePayload::Text { content, .. } => {
                assert_eq!(content, "I finished the refactoring");
            }
            _ => panic!("expected Text payload"),
        }
    }

    #[tokio::test]
    async fn send_tool_broadcast_to_peers() {
        let (_router, _parent, mut children, _dt) = setup_delegation(3, "del-tool-bcast").await;

        let args = serde_json::json!({
            "target": "broadcast",
            "content": "ready for sync",
            "message_type": "progress"
        });
        let result = send_tool::execute_send_message(&children[0], &args).await;
        assert!(
            result.display.starts_with("✓"),
            "Expected success, got: {}",
            result.display
        );

        // All children should receive the broadcast
        for (i, child) in children.iter_mut().enumerate() {
            let received = child.try_recv();
            assert!(received.is_some(), "child-{i} should receive broadcast");
        }
    }

    #[tokio::test]
    async fn send_tool_direct_to_peer() {
        let (_router, _parent, mut children, _dt) = setup_delegation(2, "del-tool-direct").await;

        let args = serde_json::json!({
            "target": "agent-1",
            "content": "review my changes",
            "message_type": "question"
        });
        let result = send_tool::execute_send_message(&children[0], &args).await;

        // Router resolves agent_id-only Direct targets via agent_id_index.
        assert!(
            result.display.starts_with("✓"),
            "expected success, got: {}",
            result.display
        );
        let received = children[1].try_recv();
        assert!(received.is_some(), "peer should have received the message");
    }

    #[tokio::test]
    async fn send_tool_missing_content_returns_error() {
        let (_router, _parent, children, _dt) = setup_delegation(1, "del-tool-err").await;

        let args = serde_json::json!({ "target": "parent" });
        let result = send_tool::execute_send_message(&children[0], &args).await;
        assert!(result.display.contains("Error"));
        assert!(result.display.contains("content"));
    }

    #[tokio::test]
    async fn send_tool_missing_target_returns_error() {
        let (_router, _parent, children, _dt) = setup_delegation(1, "del-tool-err2").await;

        let args = serde_json::json!({ "content": "hello" });
        let result = send_tool::execute_send_message(&children[0], &args).await;
        assert!(result.display.contains("Error"));
        assert!(result.display.contains("target"));
    }

    // ─── Multi-turn conversation simulation ─────────────────────────────────

    #[tokio::test]
    async fn simulate_fanout_conversation() {
        let (_router, mut parent, mut children, _dt) = setup_delegation(2, "del-convo").await;

        // Turn 1: Both children report progress
        children[0]
            .send_progress(1, 3, "working", Some("reading files".into()))
            .await
            .unwrap();
        children[1]
            .send_progress(1, 5, "working", Some("running tests".into()))
            .await
            .unwrap();

        // Parent drains messages
        let msgs = parent.drain();
        assert_eq!(msgs.len(), 2);

        // Turn 2: Child-0 discovers something and tells child-1
        let msg = AgentMessage::new(
            children[0].address.clone(),
            MessageTarget::Direct {
                address: children[1].address.clone(),
            },
            MessagePayload::Text {
                content: "Found a race condition in db.rs:42".into(),
                summary: None,
            },
        );
        children[0].send(msg).await.unwrap();

        // Child-1 receives the finding
        let finding = children[1].try_recv().unwrap();
        match &finding.payload {
            MessagePayload::Text { content, .. } => {
                assert!(content.contains("race condition"));
            }
            _ => panic!("expected text"),
        }

        // Turn 3: Both report completion to parent
        children[0]
            .send_to_parent("Fixed race condition")
            .await
            .unwrap();
        children[1].send_to_parent("Tests updated").await.unwrap();

        let final_msgs = parent.drain();
        assert_eq!(final_msgs.len(), 2);
    }

    // ─── Message ordering ───────────────────────────────────────────────────

    #[tokio::test]
    async fn messages_arrive_in_send_order() {
        let (_router, mut parent, children, _dt) = setup_delegation(1, "del-order").await;

        for i in 0..10 {
            children[0]
                .send_to_parent(format!("msg-{i}"))
                .await
                .unwrap();
        }

        let drained = parent.drain();
        assert_eq!(drained.len(), 10);
        for (i, msg) in drained.iter().enumerate() {
            match &msg.payload {
                MessagePayload::Text { content, .. } => {
                    assert_eq!(content, &format!("msg-{i}"));
                }
                _ => panic!("expected text"),
            }
        }
    }

    // ─── Ack / Nack flow ────────────────────────────────────────────────────

    #[tokio::test]
    async fn requires_ack_message_receives_ack_reply() {
        let (_router, mut parent, mut children, _dt) = setup_delegation(1, "del-ack-reply").await;

        // Child sends message with requires_ack.
        let msg = AgentMessage::new(
            children[0].address.clone(),
            MessageTarget::Parent,
            MessagePayload::Text {
                content: "need confirmation".to_string(),
                summary: None,
            },
        )
        .with_ack_required();
        assert!(msg.requires_ack);

        children[0].send(msg.clone()).await.unwrap();

        // Parent receives the message.
        let received = parent.drain();
        assert_eq!(received.len(), 1);
        assert!(received[0].requires_ack);

        // Parent sends ack back.
        let ack = received[0].make_ack(parent.address.clone());
        parent.send(ack).await.unwrap();

        // Child receives the ack.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let replies = children[0].drain();
        assert_eq!(replies.len(), 1);
        match &replies[0].payload {
            MessagePayload::Ack { message_id } => {
                assert_eq!(message_id, &msg.id);
            }
            other => panic!("expected Ack, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn nack_message_carries_reason() {
        let (_router, mut parent, mut children, _dt) = setup_delegation(1, "del-nack").await;

        let msg = AgentMessage::new(
            children[0].address.clone(),
            MessageTarget::Parent,
            MessagePayload::Text {
                content: "bad request".to_string(),
                summary: None,
            },
        )
        .with_ack_required();

        children[0].send(msg.clone()).await.unwrap();
        let received = parent.drain();
        assert_eq!(received.len(), 1);

        // Parent nacks.
        let nack =
            received[0].make_nack(parent.address.clone(), Some("invalid format".to_string()));
        parent.send(nack).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let replies = children[0].drain();
        assert_eq!(replies.len(), 1);
        match &replies[0].payload {
            MessagePayload::Nack { message_id, reason } => {
                assert_eq!(message_id, &msg.id);
                assert_eq!(reason.as_deref(), Some("invalid format"));
            }
            other => panic!("expected Nack, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn send_tool_with_requires_ack_returns_tracked_message() {
        let (_router, _parent, children, _dt) = setup_delegation(2, "del-ack-tracked").await;

        let args = serde_json::json!({
            "target": "parent",
            "content": "tracked message",
            "requires_ack": true,
        });

        let result = send_tool::execute_send_message(&children[0], &args).await;
        assert!(
            result.display.starts_with("✓"),
            "Expected success: {}",
            result.display
        );
        assert!(
            result.tracked_message.is_some(),
            "Should return tracked message"
        );
        assert!(result.tracked_message.unwrap().requires_ack);
    }

    #[tokio::test]
    async fn send_tool_without_ack_returns_no_tracked_message() {
        let (_router, _parent, children, _dt) = setup_delegation(2, "del-no-ack").await;

        let args = serde_json::json!({
            "target": "parent",
            "content": "regular message",
        });

        let result = send_tool::execute_send_message(&children[0], &args).await;
        assert!(result.display.starts_with("✓"));
        assert!(
            result.tracked_message.is_none(),
            "Should NOT track when requires_ack is false"
        );
    }

    #[tokio::test]
    async fn ack_tracker_end_to_end_with_mailbox() {
        use astra_messaging::ack_tracker::{AckConfig, PendingAckTracker};

        let (_router, mut parent, mut children, _dt) = setup_delegation(1, "del-ack-e2e").await;

        // Create a tracker for the child.
        let tracker = PendingAckTracker::with_config(AckConfig {
            ack_timeout: std::time::Duration::from_millis(200),
            max_retries: 2,
            sweep_interval: std::time::Duration::from_millis(50),
        });

        // Child sends a requires_ack message and tracks it.
        let msg = AgentMessage::new(
            children[0].address.clone(),
            MessageTarget::Parent,
            MessagePayload::Text {
                content: "important".to_string(),
                summary: None,
            },
        )
        .with_ack_required();

        children[0].send(msg.clone()).await.unwrap();
        tracker.track(std::sync::Arc::new(msg.clone())).await;
        assert_eq!(tracker.pending_count().await, 1);

        // Parent acks.
        let received = parent.drain();
        let ack = received[0].make_ack(parent.address.clone());
        parent.send(ack).await.unwrap();

        // Child receives ack and routes to tracker.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let replies = children[0].drain();
        for reply in &replies {
            if let MessagePayload::Ack { message_id } = &reply.payload {
                tracker.acknowledge(message_id).await;
            }
        }

        assert_eq!(tracker.pending_count().await, 0);
    }

    #[tokio::test]
    async fn ack_sweep_task_retries_and_dead_letters_while_idle() {
        use astra_messaging::ack_tracker::{AckConfig, PendingAckTracker, start_sweep_task};
        use astra_messaging::dead_letter::DeadLetterQueue;

        let (_router, parent, children, _dt) = setup_delegation(1, "del-ack-sweep").await;

        let tracker = Arc::new(PendingAckTracker::with_config(AckConfig {
            ack_timeout: std::time::Duration::from_millis(40),
            max_retries: 2,
            sweep_interval: std::time::Duration::from_millis(10),
        }));
        let dlq = Arc::new(DeadLetterQueue::new());
        let _sweeper = start_sweep_task(
            tracker.clone(),
            children[0].router(),
            Some(dlq.clone()),
            None,
        );

        let msg = AgentMessage::new(
            children[0].address.clone(),
            MessageTarget::Parent,
            MessagePayload::Text {
                content: "retry me while idle".to_string(),
                summary: None,
            },
        )
        .with_ack_required();

        let msg = Arc::new(msg);
        children[0].send((*msg).clone()).await.unwrap();
        tracker.track(msg.clone()).await;

        let first = tokio::time::timeout(std::time::Duration::from_secs(1), parent.recv())
            .await
            .expect("initial delivery should arrive")
            .expect("parent mailbox should stay open");
        match &first.payload {
            MessagePayload::Text { content, .. } => assert_eq!(content, "retry me while idle"),
            other => panic!("expected text payload, got {other:?}"),
        }

        let retry = tokio::time::timeout(std::time::Duration::from_secs(1), parent.recv())
            .await
            .expect("retry delivery should arrive without turn-loop sweep")
            .expect("parent mailbox should stay open");
        match &retry.payload {
            MessagePayload::Text { content, .. } => assert_eq!(content, "retry me while idle"),
            other => panic!("expected retry text payload, got {other:?}"),
        }

        tokio::time::sleep(std::time::Duration::from_millis(120)).await;

        assert_eq!(tracker.pending_count().await, 0);
        assert_eq!(dlq.count().await, 1);
        let dead_letters = dlq.list().await;
        assert_eq!(dead_letters[0].message.id, msg.id);
        match &dead_letters[0].reason {
            astra_messaging::dead_letter::DeadLetterReason::AckTimeout { attempts } => {
                assert_eq!(*attempts, 2);
            }
            other => panic!("expected AckTimeout, got: {other:?}"),
        }
    }

    // ─── Dead Letter Queue integration tests ─────────────────────────────────

    #[tokio::test]
    async fn ack_timeout_stores_in_dlq() {
        use astra_messaging::ack_tracker::{AckConfig, AckOutcome, PendingAckTracker};
        use astra_messaging::dead_letter::DeadLetterQueue;
        use std::time::Duration;

        let (_router, _parent, children, _dt) = setup_delegation(2, "del-dlq-timeout").await;

        let dlq = Arc::new(DeadLetterQueue::new());
        let tracker = PendingAckTracker::with_config(AckConfig {
            ack_timeout: Duration::from_millis(10),
            max_retries: 1, // fail after first attempt
            sweep_interval: Duration::from_millis(5),
        });

        // Send message requiring ack
        let msg = AgentMessage::new(
            children[0].address.clone(),
            MessageTarget::Direct {
                address: children[1].address.clone(),
            },
            MessagePayload::Text {
                content: "urgent task".into(),
                summary: None,
            },
        )
        .with_ack_required();

        let msg = Arc::new(msg);
        tracker.track(msg.clone()).await;
        children[0].send((*msg).clone()).await.unwrap();

        // Don't ack — wait for timeout
        tokio::time::sleep(Duration::from_millis(20)).await;
        let outcomes = tracker.sweep().await;

        // Store failed in DLQ
        for outcome in &outcomes {
            if let AckOutcome::Failed {
                message, attempts, ..
            } = outcome
            {
                dlq.store(
                    Arc::clone(message),
                    astra_messaging::dead_letter::DeadLetterReason::AckTimeout {
                        attempts: *attempts,
                    },
                    *attempts,
                )
                .await;
            }
        }

        assert_eq!(dlq.count().await, 1);
        let entries = dlq.list().await;
        assert_eq!(entries[0].message.id, msg.id);
    }

    #[tokio::test]
    async fn nack_stores_in_dlq() {
        use astra_messaging::ack_tracker::{AckOutcome, PendingAckTracker};
        use astra_messaging::dead_letter::DeadLetterQueue;

        let (_router, _parent, children, _dt) = setup_delegation(2, "del-dlq-nack").await;

        let dlq = Arc::new(DeadLetterQueue::new());
        let tracker = PendingAckTracker::new();

        let msg = AgentMessage::new(
            children[0].address.clone(),
            MessageTarget::Direct {
                address: children[1].address.clone(),
            },
            MessagePayload::Text {
                content: "bad request".into(),
                summary: None,
            },
        )
        .with_ack_required();

        let msg = Arc::new(msg);
        let msg_id = msg.id.clone();
        tracker.track(msg.clone()).await;
        children[0].send((*msg).clone()).await.unwrap();

        // Receiver nacks
        tracker.reject(&msg_id, Some("invalid format".into())).await;

        let failures = tracker.failed_outcomes().await;
        for outcome in &failures {
            if let AckOutcome::Rejected {
                message, reason, ..
            } = outcome
            {
                dlq.store(
                    Arc::clone(message),
                    astra_messaging::dead_letter::DeadLetterReason::Rejected {
                        reason: reason.clone(),
                    },
                    1,
                )
                .await;
            }
        }

        assert_eq!(dlq.count().await, 1);
        let entries = dlq.list().await;
        match &entries[0].reason {
            astra_messaging::dead_letter::DeadLetterReason::Rejected { reason } => {
                assert_eq!(reason.as_deref(), Some("invalid format"));
            }
            _ => panic!("expected Rejected reason"),
        }
    }

    #[tokio::test]
    async fn dlq_take_for_retry_removes_entries() {
        use astra_messaging::ack_tracker::{AckConfig, AckOutcome, PendingAckTracker};
        use astra_messaging::dead_letter::DeadLetterQueue;
        use std::time::Duration;

        let (_router, _parent, children, _dt) = setup_delegation(2, "del-dlq-retry").await;

        let dlq = Arc::new(DeadLetterQueue::new());
        let tracker = PendingAckTracker::with_config(AckConfig {
            ack_timeout: Duration::from_millis(5),
            max_retries: 1,
            sweep_interval: Duration::from_millis(5),
        });

        // Send 3 messages, all will timeout
        for i in 0..3 {
            let msg = AgentMessage::new(
                children[0].address.clone(),
                MessageTarget::Direct {
                    address: children[1].address.clone(),
                },
                MessagePayload::Text {
                    content: format!("msg-{i}"),
                    summary: None,
                },
            )
            .with_ack_required();
            tracker.track(Arc::new(msg)).await;
        }

        tokio::time::sleep(Duration::from_millis(15)).await;
        let outcomes = tracker.sweep().await;
        for outcome in &outcomes {
            if let AckOutcome::Failed {
                message, attempts, ..
            } = outcome
            {
                dlq.store(
                    Arc::clone(message),
                    astra_messaging::dead_letter::DeadLetterReason::AckTimeout {
                        attempts: *attempts,
                    },
                    *attempts,
                )
                .await;
            }
        }

        assert_eq!(dlq.count().await, 3);

        // Take first 2 for retry by their IDs
        let all = dlq.list().await;
        let id0 = all[0].message.id.clone();
        let id1 = all[1].message.id.clone();

        let r0 = dlq.take_for_retry(&id0).await;
        assert!(r0.is_some());
        let r1 = dlq.take_for_retry(&id1).await;
        assert!(r1.is_some());
        assert_eq!(dlq.count().await, 1);
    }

    // ─── Metrics integration tests ───────────────────────────────────────────

    #[tokio::test]
    async fn metrics_track_send_receive_ack_flow() {
        use astra_messaging::metrics::MessagingMetrics;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let (_router, _parent, mut children, _dt) = setup_delegation(2, "del-metrics").await;

        let metrics = Arc::new(MessagingMetrics::new());

        // Send
        let msg = AgentMessage::new(
            children[0].address.clone(),
            MessageTarget::Direct {
                address: children[1].address.clone(),
            },
            MessagePayload::Text {
                content: "hello".into(),
                summary: None,
            },
        );
        children[0].send(msg).await.unwrap();
        metrics.messages_sent.fetch_add(1, Ordering::Relaxed);

        // Receive
        let received = children[1].try_recv().unwrap();
        metrics.messages_received.fetch_add(1, Ordering::Relaxed);

        // Simulate ack latency
        let start = std::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let ack_msg = received.make_ack(children[1].address.clone());
        children[1].send(ack_msg).await.unwrap();
        metrics.acks_sent.fetch_add(1, Ordering::Relaxed);
        metrics.ack_latency.record(start.elapsed());

        let snap = metrics.snapshot();
        assert_eq!(snap.messages_sent, 1);
        assert_eq!(snap.messages_received, 1);
        assert_eq!(snap.acks_sent, 1);
        assert!(snap.ack_latency.count > 0);
        // Latency should be non-zero (we slept 5ms, but don't assert exact bound)
        assert!(snap.ack_latency.min_us > 0);
    }

    #[tokio::test]
    async fn event_dispatcher_receives_messaging_events() {
        use astra_messaging::metrics::{EventDispatcher, MessagingEvent, MessagingEventHandler};
        use std::sync::atomic::{AtomicU32, Ordering};

        struct Counter {
            sent: AtomicU32,
            received: AtomicU32,
            dead_lettered: AtomicU32,
        }
        impl MessagingEventHandler for Counter {
            fn on_event(&self, event: &MessagingEvent) {
                match event {
                    MessagingEvent::Sent { .. } => {
                        self.sent.fetch_add(1, Ordering::Relaxed);
                    }
                    MessagingEvent::Received { .. } => {
                        self.received.fetch_add(1, Ordering::Relaxed);
                    }
                    MessagingEvent::DeadLettered { .. } => {
                        self.dead_lettered.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        }

        let (_router, _parent, children, _dt) = setup_delegation(2, "del-events").await;

        let dispatcher = EventDispatcher::new();
        let counter = Arc::new(Counter {
            sent: AtomicU32::new(0),
            received: AtomicU32::new(0),
            dead_lettered: AtomicU32::new(0),
        });
        dispatcher.add_handler(counter.clone()).await;

        // Fire events
        dispatcher
            .dispatch(&MessagingEvent::Sent {
                message_id: "m1".into(),
                from: children[0].address.clone(),
                to: MessageTarget::Direct {
                    address: children[1].address.clone(),
                },
            })
            .await;

        dispatcher
            .dispatch(&MessagingEvent::Received {
                message_id: "m1".into(),
                from: children[0].address.clone(),
                to: children[1].address.clone(),
            })
            .await;

        dispatcher
            .dispatch(&MessagingEvent::DeadLettered {
                message_id: "m1".into(),
                reason: "ack timeout".into(),
            })
            .await;

        assert_eq!(counter.sent.load(Ordering::Relaxed), 1);
        assert_eq!(counter.received.load(Ordering::Relaxed), 1);
        assert_eq!(counter.dead_lettered.load(Ordering::Relaxed), 1);
    }

    // ── Concurrent stress tests ─────────────────────────────────────────────

    /// Stress test: N senders concurrently send M messages each to a single receiver.
    /// Verifies: no lost messages, correct delivery, no panics under contention.
    #[tokio::test]
    async fn stress_concurrent_senders_single_receiver() {
        const NUM_SENDERS: usize = 10;
        const MSGS_PER_SENDER: usize = 100;
        const TOTAL_MSGS: usize = NUM_SENDERS * MSGS_PER_SENDER;

        let transport = Arc::new(InProcessTransport::new());
        let dt = tracker();
        let router = Arc::new(AgentMailboxRouter::new(transport, dt.clone()));

        // Register receiver
        let receiver_addr = addr("run-recv", "receiver");
        let receiver_mb = router.register(receiver_addr.clone(), None).await.unwrap();

        // Register senders and record relationships
        let mut sender_mbs = Vec::new();
        for i in 0..NUM_SENDERS {
            let sender_run = format!("run-sender-{i}");
            let sender_addr = addr(&sender_run, "sender");
            let mb = router.register(sender_addr.clone(), None).await.unwrap();
            sender_mbs.push((sender_addr, mb));

            // Record parent→sender relationship for authorization
            dt.record_sub_run(SubRunRecord {
                run_id: sender_run.clone(),
                parent_run_id: "run-recv".into(),
                delegation_id: "stress-test".into(),
                agent_id: "sender".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

            // Record parent→receiver (mutual visibility for replies)
            dt.record_sub_run(SubRunRecord {
                run_id: "run-recv".into(),
                parent_run_id: sender_run.clone(),
                delegation_id: "stress-test".into(),
                agent_id: "receiver".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        }

        // Spawn concurrent senders
        let mut handles = Vec::new();
        for (i, (sender_addr, sender_mb)) in sender_mbs.into_iter().enumerate() {
            let recv_addr = receiver_addr.clone();
            let router_clone = router.clone();
            handles.push(tokio::spawn(async move {
                for j in 0..MSGS_PER_SENDER {
                    let msg = AgentMessage::new(
                        sender_addr.clone(),
                        MessageTarget::Direct {
                            address: recv_addr.clone(),
                        },
                        MessagePayload::Text {
                            content: format!("Hello from sender {i} msg {j}"),
                            summary: None,
                        },
                    );
                    if let Err(e) = router_clone.send(msg).await {
                        panic!("Send failed for sender {i} msg {j}: {e}");
                    }
                }
                sender_mb // Return to drop after all sends complete
            }));
        }

        // Wait for all senders to complete
        for handle in handles {
            let _ = handle.await.unwrap();
        }

        // Verify receiver got all messages
        let mut received = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while received < TOTAL_MSGS {
            match tokio::time::timeout(
                deadline.saturating_duration_since(std::time::Instant::now()),
                receiver_mb.recv(),
            )
            .await
            {
                Ok(Some(_msg)) => received += 1,
                Ok(None) => break, // Channel closed
                Err(_) => break,   // Timeout
            }
        }

        assert_eq!(
            received, TOTAL_MSGS,
            "Expected {TOTAL_MSGS} messages, got {received}"
        );
    }

    /// Stress test: N senders, M receivers, each sender broadcasts to all receivers.
    /// Verifies: fanout correctness, no message loss in multi-receiver scenario.
    #[tokio::test]
    async fn stress_broadcast_to_multiple_receivers() {
        const NUM_SENDERS: usize = 5;
        const NUM_RECEIVERS: usize = 5;
        const MSGS_PER_SENDER: usize = 20;
        const TOTAL_PER_RECEIVER: usize = NUM_SENDERS * MSGS_PER_SENDER;

        let transport = Arc::new(InProcessTransport::new());
        let dt = tracker();
        let router = Arc::new(AgentMailboxRouter::new(transport, dt.clone()));

        // Register receivers
        let mut receiver_mbs = Vec::new();
        for i in 0..NUM_RECEIVERS {
            let recv_addr = addr(&format!("run-recv-{i}"), "receiver");
            let mb = router.register(recv_addr.clone(), None).await.unwrap();
            receiver_mbs.push((recv_addr, mb));
        }

        // Register senders and set up relationships
        let mut sender_addrs = Vec::new();
        for i in 0..NUM_SENDERS {
            let sender_run = format!("run-sender-{i}");
            let sender_addr = addr(&sender_run, "sender");
            router.register(sender_addr.clone(), None).await.unwrap();
            sender_addrs.push(sender_addr);

            // Record relationships for all receivers
            for j in 0..NUM_RECEIVERS {
                let recv_run = format!("run-recv-{j}");
                dt.record_sub_run(SubRunRecord {
                    run_id: recv_run,
                    parent_run_id: sender_run.clone(),
                    delegation_id: "broadcast".into(),
                    agent_id: "receiver".into(),
                    depth: 1,
                    state: SubRunState::Created,
                    retry_of: None,
                })
                .await;
            }
        }

        // Spawn concurrent senders, each sending to all receivers
        let mut handles = Vec::new();
        for (i, sender_addr) in sender_addrs.into_iter().enumerate() {
            let router_clone = router.clone();
            let receivers: Vec<_> = receiver_mbs.iter().map(|(a, _)| a.clone()).collect();
            handles.push(tokio::spawn(async move {
                for j in 0..MSGS_PER_SENDER {
                    for recv_addr in &receivers {
                        let msg = AgentMessage::new(
                            sender_addr.clone(),
                            MessageTarget::Direct {
                                address: recv_addr.clone(),
                            },
                            MessagePayload::Text {
                                content: format!("Broadcast {i}-{j}"),
                                summary: None,
                            },
                        );
                        router_clone.send(msg).await.unwrap();
                    }
                }
            }));
        }

        // Wait for senders
        for handle in handles {
            handle.await.unwrap();
        }

        // Verify each receiver got all messages
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        for (i, (_addr, mb)) in receiver_mbs.iter().enumerate() {
            let mut received = 0;
            while received < TOTAL_PER_RECEIVER {
                match tokio::time::timeout(
                    deadline.saturating_duration_since(std::time::Instant::now()),
                    mb.recv(),
                )
                .await
                {
                    Ok(Some(_)) => received += 1,
                    Ok(None) | Err(_) => break,
                }
            }
            assert_eq!(
                received, TOTAL_PER_RECEIVER,
                "Receiver {i}: expected {TOTAL_PER_RECEIVER}, got {received}"
            );
        }
    }

    /// Stress test: concurrent send + ack + retry + DLQ interactions.
    /// Verifies: system stability under mixed operations.
    #[tokio::test]
    async fn stress_mixed_operations() {
        use astra_messaging::ack_tracker::PendingAckTracker;
        use astra_messaging::dead_letter::DeadLetterQueue;

        const NUM_AGENTS: usize = 5;
        const OPS_PER_AGENT: usize = 50;

        let transport = Arc::new(InProcessTransport::new());
        let dt = tracker();
        let router = Arc::new(AgentMailboxRouter::new(transport.clone(), dt.clone()));
        let ack_tracker = Arc::new(PendingAckTracker::new());
        let dlq = Arc::new(DeadLetterQueue::new());

        // Register agents in a ring topology
        let mut agents = Vec::new();
        for i in 0..NUM_AGENTS {
            let agent_addr = addr(&format!("run-{i}"), &format!("agent-{i}"));
            let mb = router.register(agent_addr.clone(), None).await.unwrap();
            agents.push((agent_addr, mb));

            // Each agent can send to the next
            let next = (i + 1) % NUM_AGENTS;
            dt.record_sub_run(SubRunRecord {
                run_id: format!("run-{next}"),
                parent_run_id: format!("run-{i}"),
                delegation_id: "ring".into(),
                agent_id: format!("agent-{next}"),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        }

        // Spawn mixed operations
        let mut handles = Vec::new();
        for i in 0..NUM_AGENTS {
            let sender_addr = agents[i].0.clone();
            let recv_addr = agents[(i + 1) % NUM_AGENTS].0.clone();
            let router_clone = router.clone();
            let ack_clone = ack_tracker.clone();
            let dlq_clone = dlq.clone();

            handles.push(tokio::spawn(async move {
                for j in 0..OPS_PER_AGENT {
                    let msg_id = format!("msg-{i}-{j}");

                    // Create and send a message via router
                    let mut msg = AgentMessage::new(
                        sender_addr.clone(),
                        MessageTarget::Direct {
                            address: recv_addr.clone(),
                        },
                        MessagePayload::Text {
                            content: format!("Mixed op {i}-{j}"),
                            summary: None,
                        },
                    );
                    // Override the auto-generated ID for tracking
                    msg.id = msg_id.clone();
                    msg.requires_ack = true;

                    let msg = Arc::new(msg);
                    router_clone.send((*msg).clone()).await.unwrap();
                    ack_clone.track(msg.clone()).await;

                    // Randomly: ack, nack, or let timeout (simulate DLQ)
                    match j % 3 {
                        0 => {
                            // Ack
                            ack_clone.acknowledge(&msg_id).await;
                        }
                        1 => {
                            // Nack (reject)
                            ack_clone
                                .reject(&msg_id, Some("test rejection".to_string()))
                                .await;
                        }
                        _ => {
                            // Simulate dead-letter after "timeout"
                            dlq_clone
                                .store(
                                    msg,
                                    astra_messaging::dead_letter::DeadLetterReason::AckTimeout {
                                        attempts: 3,
                                    },
                                    3,
                                )
                                .await;
                        }
                    }
                }
            }));
        }

        // Wait for all operations
        for handle in handles {
            handle.await.unwrap();
        }

        // Verify system didn't panic and DLQ has expected entries
        let dlq_summary = dlq.reason_summary().await;
        // We expect ~1/3 of total messages to be dead-lettered (every j%3==2)
        // Due to integer rounding, allow some slack
        let expected_dlq = (NUM_AGENTS * OPS_PER_AGENT) / 3;
        assert!(
            dlq_summary.total >= expected_dlq - 5 && dlq_summary.total <= expected_dlq + 5,
            "Expected ~{expected_dlq} dead letters, got {}",
            dlq_summary.total
        );
    }
}
