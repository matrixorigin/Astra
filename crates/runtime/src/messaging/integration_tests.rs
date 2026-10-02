//! Integration tests for inter-agent messaging.
//!
//! These tests verify end-to-end messaging flows that span multiple components:
//! router + transport + delegation tracker.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::server::delegation::engine::{DelegationTracker, SubRunRecord, SubRunState};
    use astra_messaging::in_process::InProcessTransport;
    use astra_messaging::router::{AgentMailbox, AgentMailboxRouter};
    use astra_messaging::types::*;

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

    // ─── Metrics integration tests ───────────────────────────────────────────

    #[tokio::test]
    async fn metrics_track_send_receive_flow() {
        use astra_messaging::metrics::MessagingMetrics;
        use std::sync::atomic::Ordering;

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

        let snap = metrics.snapshot();
        assert_eq!(snap.messages_sent, 1);
        assert_eq!(snap.messages_received, 1);
        assert!(matches!(received.payload, MessagePayload::Text { .. }));
        assert!(children[0].try_recv().is_none());
    }

    #[tokio::test]
    async fn event_dispatcher_receives_messaging_events() {
        use astra_messaging::metrics::{EventDispatcher, MessagingEvent, MessagingEventHandler};
        use std::sync::atomic::{AtomicU32, Ordering};

        struct Counter {
            sent: AtomicU32,
            received: AtomicU32,
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
                    _ => {}
                }
            }
        }

        let (_router, _parent, children, _dt) = setup_delegation(2, "del-events").await;

        let dispatcher = EventDispatcher::new();
        let counter = Arc::new(Counter {
            sent: AtomicU32::new(0),
            received: AtomicU32::new(0),
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

        assert_eq!(counter.sent.load(Ordering::Relaxed), 1);
        assert_eq!(counter.received.load(Ordering::Relaxed), 1);
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
}
