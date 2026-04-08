//! Integration tests for inter-agent messaging.
//!
//! These tests verify end-to-end messaging flows that span multiple components:
//! router + transport + delegation tracker + send_tool.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::messaging::in_process::InProcessTransport;
    use crate::messaging::router::{AgentMailbox, AgentMailboxRouter};
    use crate::messaging::send_tool;
    use crate::messaging::types::*;
    use crate::server::delegation_engine::{DelegationTracker, SubRunRecord};

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
        let parent_mb = router
            .register(parent_addr.clone(), None)
            .await
            .unwrap();

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
        let (_router, _parent, mut children, _dt) =
            setup_delegation(3, "del-fanout").await;

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
        let (_router, _parent, mut children, _dt) =
            setup_delegation(3, "del-broadcast").await;

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
        let (_router, mut parent, children, _dt) =
            setup_delegation(2, "del-parent").await;

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
        let (_router, mut parent, children, _dt) =
            setup_delegation(1, "del-progress").await;

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
        let (_router, parent, mut children, _dt) =
            setup_delegation(2, "del-parent-to-child").await;

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
        let (_router, mut parent, children, _dt) =
            setup_delegation(1, "del-tool").await;

        let args = serde_json::json!({
            "target": "parent",
            "content": "I finished the refactoring"
        });
        let result = send_tool::execute_send_message(&children[0], &args).await;
        assert!(result.display.starts_with("✓"), "Expected success, got: {}", result.display);

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
        let (_router, _parent, mut children, _dt) =
            setup_delegation(3, "del-tool-bcast").await;

        let args = serde_json::json!({
            "target": "broadcast",
            "content": "ready for sync",
            "message_type": "progress"
        });
        let result = send_tool::execute_send_message(&children[0], &args).await;
        assert!(result.display.starts_with("✓"), "Expected success, got: {}", result.display);

        // All children should receive the broadcast
        for (i, child) in children.iter_mut().enumerate() {
            let received = child.try_recv();
            assert!(received.is_some(), "child-{i} should receive broadcast");
        }
    }

    #[tokio::test]
    async fn send_tool_direct_to_peer() {
        let (_router, _parent, mut children, _dt) =
            setup_delegation(2, "del-tool-direct").await;

        let args = serde_json::json!({
            "target": "agent-1",
            "content": "review my changes",
            "message_type": "question"
        });
        let result = send_tool::execute_send_message(&children[0], &args).await;

        // Router resolves agent_id-only Direct targets via agent_id_index.
        assert!(
            result.display.starts_with("✓"),
            "expected success, got: {}", result.display
        );
        let received = children[1].try_recv();
        assert!(received.is_some(), "peer should have received the message");
    }

    #[tokio::test]
    async fn send_tool_missing_content_returns_error() {
        let (_router, _parent, children, _dt) =
            setup_delegation(1, "del-tool-err").await;

        let args = serde_json::json!({ "target": "parent" });
        let result = send_tool::execute_send_message(&children[0], &args).await;
        assert!(result.display.contains("Error"));
        assert!(result.display.contains("content"));
    }

    #[tokio::test]
    async fn send_tool_missing_target_returns_error() {
        let (_router, _parent, children, _dt) =
            setup_delegation(1, "del-tool-err2").await;

        let args = serde_json::json!({ "content": "hello" });
        let result = send_tool::execute_send_message(&children[0], &args).await;
        assert!(result.display.contains("Error"));
        assert!(result.display.contains("target"));
    }

    // ─── Multi-turn conversation simulation ─────────────────────────────────

    #[tokio::test]
    async fn simulate_fanout_conversation() {
        let (_router, mut parent, mut children, _dt) =
            setup_delegation(2, "del-convo").await;

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
        children[0].send_to_parent("Fixed race condition").await.unwrap();
        children[1].send_to_parent("Tests updated").await.unwrap();

        let final_msgs = parent.drain();
        assert_eq!(final_msgs.len(), 2);
    }

    // ─── Message ordering ───────────────────────────────────────────────────

    #[tokio::test]
    async fn messages_arrive_in_send_order() {
        let (_router, mut parent, children, _dt) =
            setup_delegation(1, "del-order").await;

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
        let (_router, mut parent, mut children, _dt) =
            setup_delegation(1, "del-ack-reply").await;

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
        let (_router, mut parent, mut children, _dt) =
            setup_delegation(1, "del-nack").await;

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
        let nack = received[0].make_nack(
            parent.address.clone(),
            Some("invalid format".to_string()),
        );
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
        let (_router, _parent, children, _dt) =
            setup_delegation(2, "del-ack-tracked").await;

        let args = serde_json::json!({
            "target": "parent",
            "content": "tracked message",
            "requires_ack": true,
        });

        let result = send_tool::execute_send_message(&children[0], &args).await;
        assert!(result.display.starts_with("✓"), "Expected success: {}", result.display);
        assert!(result.tracked_message.is_some(), "Should return tracked message");
        assert!(result.tracked_message.unwrap().requires_ack);
    }

    #[tokio::test]
    async fn send_tool_without_ack_returns_no_tracked_message() {
        let (_router, _parent, children, _dt) =
            setup_delegation(2, "del-no-ack").await;

        let args = serde_json::json!({
            "target": "parent",
            "content": "regular message",
        });

        let result = send_tool::execute_send_message(&children[0], &args).await;
        assert!(result.display.starts_with("✓"));
        assert!(result.tracked_message.is_none(), "Should NOT track when requires_ack is false");
    }

    #[tokio::test]
    async fn ack_tracker_end_to_end_with_mailbox() {
        use crate::messaging::ack_tracker::{AckConfig, PendingAckTracker};

        let (_router, mut parent, mut children, _dt) =
            setup_delegation(1, "del-ack-e2e").await;

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
}
