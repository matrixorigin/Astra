//! Database transport integration tests.
//!
//! These tests require a running MatrixOne/MySQL instance. They are gated
//! behind the `MO_TEST_DB` environment variable:
//!
//! ```sh
//! MO_TEST_DB="mysql://root:111@127.0.0.1:6001/astra_test" cargo test -p astra-runtime db_transport_integration
//! ```
//!
//! If `MO_TEST_DB` is not set, all tests in this module are skipped.

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::messaging::db_transport::{DatabaseTransport, ensure_schema};
    use crate::messaging::transport::MessageTransport;
    use crate::messaging::types::*;

    fn addr(run: &str, agent: &str) -> AgentAddress {
        AgentAddress::new(run, agent)
    }

    /// Connect to test DB or skip the test.
    async fn test_pool() -> Option<sqlx::Pool<sqlx::MySql>> {
        let url = match std::env::var("MO_TEST_DB") {
            Ok(u) => u,
            Err(_) => return None,
        };
        match sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(3))
            .connect(&url)
            .await
        {
            Ok(pool) => Some(pool),
            Err(e) => {
                eprintln!("⚠ DB connection failed (skipping): {e}");
                None
            }
        }
    }

    /// Clean up test messages between tests.
    async fn cleanup(pool: &sqlx::Pool<sqlx::MySql>) {
        let _ = sqlx::query("DELETE FROM agent_message_queue WHERE message_id LIKE 'test-%' OR from_run_id LIKE 'run-%'")
            .execute(pool)
            .await;
    }

    macro_rules! skip_without_db {
        ($pool:ident) => {
            let Some($pool) = test_pool().await else {
                eprintln!("⚠ MO_TEST_DB not set, skipping");
                return;
            };
            ensure_schema(&$pool).await.expect("schema creation failed");
            cleanup(&$pool).await;
        };
    }

    // ── Schema ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn ensure_schema_is_idempotent() {
        skip_without_db!(pool);
        // Call twice — should not error.
        ensure_schema(&pool).await.unwrap();
        ensure_schema(&pool).await.unwrap();
    }

    // ── Direct Messages ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn direct_message_send_and_receive() {
        skip_without_db!(pool);

        let transport = DatabaseTransport::new(pool.clone())
            .with_poll_interval(Duration::from_millis(50));

        let a = addr("run-db-1", "sender");
        let b = addr("run-db-2", "receiver");

        transport.register(a.clone(), None).await.unwrap();
        transport.register(b.clone(), None).await.unwrap();

        let mut stream_b = transport.subscribe(&b).await.unwrap();

        let msg = Arc::new(AgentMessage::new(
            a.clone(),
            MessageTarget::Direct { address: b.clone() },
            MessagePayload::Text {
                content: "hello from DB".into(),
                summary: None,
            },
        ));
        transport.send(msg).await.unwrap();

        // Wait for poll to deliver.
        let received = tokio::time::timeout(Duration::from_secs(2), stream_b.recv())
            .await
            .expect("timeout waiting for message")
            .expect("stream closed");

        assert_eq!(received.from.agent_id, "sender");
        match &received.payload {
            MessagePayload::Text { content, .. } => {
                assert_eq!(content, "hello from DB");
            }
            other => panic!("expected Text, got: {other:?}"),
        }

        cleanup(&pool).await;
    }

    // ── Broadcast ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn broadcast_delivers_to_all_members() {
        skip_without_db!(pool);

        let transport = DatabaseTransport::new(pool.clone())
            .with_poll_interval(Duration::from_millis(50));

        let leader = addr("run-db-lead", "leader");
        let w1 = addr("run-db-w1", "worker-1");
        let w2 = addr("run-db-w2", "worker-2");
        let del = "del-db-test";

        transport.register(leader.clone(), Some(del.into())).await.unwrap();
        transport.register(w1.clone(), Some(del.into())).await.unwrap();
        transport.register(w2.clone(), Some(del.into())).await.unwrap();

        let mut stream_w1 = transport.subscribe(&w1).await.unwrap();
        let mut stream_w2 = transport.subscribe(&w2).await.unwrap();
        let mut stream_leader = transport.subscribe(&leader).await.unwrap();

        let msg = Arc::new(AgentMessage::new(
            leader.clone(),
            MessageTarget::Broadcast { delegation_id: del.into() },
            MessagePayload::Signal(AgentSignal::Heartbeat),
        ));
        transport.broadcast(del, msg).await.unwrap();

        // All members should receive (including sender, matching InProcessTransport behavior).
        let r1 = tokio::time::timeout(Duration::from_secs(2), stream_w1.recv()).await;
        let r2 = tokio::time::timeout(Duration::from_secs(2), stream_w2.recv()).await;
        let r_leader = tokio::time::timeout(Duration::from_secs(2), stream_leader.recv()).await;

        assert!(r1.is_ok() && r1.unwrap().is_some(), "worker-1 should receive broadcast");
        assert!(r2.is_ok() && r2.unwrap().is_some(), "worker-2 should receive broadcast");
        assert!(r_leader.is_ok() && r_leader.unwrap().is_some(), "leader should receive own broadcast");

        cleanup(&pool).await;
    }

    // ── Multiple Messages in Order ──────────────────────────────────────────

    #[tokio::test]
    async fn messages_arrive_in_fifo_order() {
        skip_without_db!(pool);

        let transport = DatabaseTransport::new(pool.clone())
            .with_poll_interval(Duration::from_millis(50));

        let a = addr("run-db-order-a", "alice");
        let b = addr("run-db-order-b", "bob");

        transport.register(a.clone(), None).await.unwrap();
        transport.register(b.clone(), None).await.unwrap();

        let mut stream_b = transport.subscribe(&b).await.unwrap();

        // Send 5 messages in order.
        for i in 0..5 {
            let msg = Arc::new(AgentMessage::new(
                a.clone(),
                MessageTarget::Direct { address: b.clone() },
                MessagePayload::Text {
                    content: format!("msg-{i}"),
                    summary: None,
                },
            ));
            transport.send(msg).await.unwrap();
        }

        // Receive them and verify order.
        let mut received = Vec::new();
        for _ in 0..5 {
            let msg = tokio::time::timeout(Duration::from_secs(3), stream_b.recv())
                .await
                .expect("timeout")
                .expect("stream closed");
            if let MessagePayload::Text { content, .. } = &msg.payload {
                received.push(content.clone());
            }
        }
        assert_eq!(received, vec!["msg-0", "msg-1", "msg-2", "msg-3", "msg-4"]);

        cleanup(&pool).await;
    }

    // ── TTL Expiry ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn expired_messages_not_delivered() {
        skip_without_db!(pool);

        let transport = DatabaseTransport::new(pool.clone())
            .with_poll_interval(Duration::from_millis(50));

        let a = addr("run-db-ttl-a", "alice");
        let b = addr("run-db-ttl-b", "bob");

        transport.register(a.clone(), None).await.unwrap();
        transport.register(b.clone(), None).await.unwrap();

        // Send a message with TTL=0 (immediately expired).
        let msg = AgentMessage::new(
            a.clone(),
            MessageTarget::Direct { address: b.clone() },
            MessagePayload::Text {
                content: "expired".into(),
                summary: None,
            },
        )
        .with_ttl(Duration::ZERO);

        transport.send(Arc::new(msg)).await.unwrap();

        // Also send a non-expired message.
        let msg2 = Arc::new(AgentMessage::new(
            a.clone(),
            MessageTarget::Direct { address: b.clone() },
            MessagePayload::Text {
                content: "alive".into(),
                summary: None,
            },
        ));
        transport.send(msg2).await.unwrap();

        let mut stream_b = transport.subscribe(&b).await.unwrap();

        // Should only receive the non-expired message.
        let received = tokio::time::timeout(Duration::from_secs(2), stream_b.recv())
            .await
            .expect("timeout")
            .expect("stream closed");

        match &received.payload {
            MessagePayload::Text { content, .. } => {
                assert_eq!(content, "alive", "expired message should be filtered");
            }
            _ => panic!("unexpected payload"),
        }

        cleanup(&pool).await;
    }

    // ── Cleanup Utilities ───────────────────────────────────────────────────

    #[tokio::test]
    async fn cleanup_expired_removes_old_messages() {
        skip_without_db!(pool);

        let transport = DatabaseTransport::new(pool.clone())
            .with_poll_interval(Duration::from_millis(50));

        let a = addr("run-db-clean-a", "alice");
        let b = addr("run-db-clean-b", "bob");

        transport.register(a.clone(), None).await.unwrap();

        // Insert an expired message.
        let msg = AgentMessage::new(
            a.clone(),
            MessageTarget::Direct { address: b.clone() },
            MessagePayload::Text {
                content: "old".into(),
                summary: None,
            },
        )
        .with_ttl(Duration::ZERO);
        transport.send(Arc::new(msg)).await.unwrap();

        let removed = transport.cleanup_expired().await.unwrap();
        assert!(removed >= 1, "should have removed at least 1 expired message");

        cleanup(&pool).await;
    }

    // ── Unregister stops stream ─────────────────────────────────────────────

    #[tokio::test]
    async fn unregister_stops_poll_task() {
        skip_without_db!(pool);

        let transport = DatabaseTransport::new(pool.clone())
            .with_poll_interval(Duration::from_millis(50));

        let a = addr("run-db-unreg", "agent");
        transport.register(a.clone(), None).await.unwrap();
        assert_eq!(transport.agent_count().await, 1);

        transport.unregister(&a).await.unwrap();
        assert_eq!(transport.agent_count().await, 0);

        cleanup(&pool).await;
    }

    // ── Subscribe requires registration ─────────────────────────────────────

    #[tokio::test]
    async fn subscribe_requires_registration() {
        skip_without_db!(pool);

        let transport = DatabaseTransport::new(pool.clone());
        let a = addr("run-db-noreg", "ghost");

        let result = transport.subscribe(&a).await;
        assert!(result.is_err(), "subscribe without register should fail");
    }
}
