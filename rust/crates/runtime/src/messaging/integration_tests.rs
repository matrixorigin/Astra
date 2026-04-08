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
}
