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

    use crate::messaging::db_transport::{
        DatabaseTransport, ensure_schema, mark_direct_failed_by_identity,
    };
    use crate::messaging::transport::MessageTransport;
    use crate::messaging::types::*;
    use sqlx::Row;

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

        let transport =
            DatabaseTransport::new(pool.clone()).with_poll_interval(Duration::from_millis(50));

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

        let transport =
            DatabaseTransport::new(pool.clone()).with_poll_interval(Duration::from_millis(50));

        let leader = addr("run-db-lead", "leader");
        let w1 = addr("run-db-w1", "worker-1");
        let w2 = addr("run-db-w2", "worker-2");
        let del = "del-db-test";

        transport
            .register(leader.clone(), Some(del.into()))
            .await
            .unwrap();
        transport
            .register(w1.clone(), Some(del.into()))
            .await
            .unwrap();
        transport
            .register(w2.clone(), Some(del.into()))
            .await
            .unwrap();

        let mut stream_w1 = transport.subscribe(&w1).await.unwrap();
        let mut stream_w2 = transport.subscribe(&w2).await.unwrap();
        let mut stream_leader = transport.subscribe(&leader).await.unwrap();

        let msg = Arc::new(AgentMessage::new(
            leader.clone(),
            MessageTarget::Broadcast {
                delegation_id: del.into(),
            },
            MessagePayload::Signal(AgentSignal::Heartbeat),
        ));
        transport.broadcast(del, msg).await.unwrap();

        // All members should receive (including sender, matching InProcessTransport behavior).
        let r1 = tokio::time::timeout(Duration::from_secs(2), stream_w1.recv()).await;
        let r2 = tokio::time::timeout(Duration::from_secs(2), stream_w2.recv()).await;
        let r_leader = tokio::time::timeout(Duration::from_secs(2), stream_leader.recv()).await;

        assert!(
            r1.is_ok() && r1.unwrap().is_some(),
            "worker-1 should receive broadcast"
        );
        assert!(
            r2.is_ok() && r2.unwrap().is_some(),
            "worker-2 should receive broadcast"
        );
        assert!(
            r_leader.is_ok() && r_leader.unwrap().is_some(),
            "leader should receive own broadcast"
        );

        cleanup(&pool).await;
    }

    // ── Multiple Messages in Order ──────────────────────────────────────────

    #[tokio::test]
    async fn messages_arrive_in_fifo_order() {
        skip_without_db!(pool);

        let transport =
            DatabaseTransport::new(pool.clone()).with_poll_interval(Duration::from_millis(50));

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
    async fn expired_direct_messages_are_marked_failed() {
        skip_without_db!(pool);

        let transport =
            DatabaseTransport::new(pool.clone()).with_poll_interval(Duration::from_millis(50));

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
        let expired_message_id = msg.id.clone();

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

        let status = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let row =
                    sqlx::query("SELECT status FROM agent_message_queue WHERE message_id = ?")
                        .bind(&expired_message_id)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                let status: String = row.try_get("status").unwrap();
                if status == "failed" {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("expired direct message should eventually be dead-lettered");

        assert_eq!(status, "failed");

        cleanup(&pool).await;
    }

    #[tokio::test]
    async fn invalid_direct_payloads_are_marked_failed() {
        skip_without_db!(pool);

        let transport =
            DatabaseTransport::new(pool.clone()).with_poll_interval(Duration::from_millis(50));

        let sender = addr("run-db-bad-json-a", "alice");
        let receiver = addr("run-db-bad-json-b", "bob");

        transport.register(sender.clone(), None).await.unwrap();
        transport.register(receiver.clone(), None).await.unwrap();

        let mut stream_b = transport.subscribe(&receiver).await.unwrap();
        let message_id = "test-invalid-direct-json";

        sqlx::query(
            "INSERT INTO agent_message_queue
             (message_id, from_run_id, from_agent_id, to_run_id, to_agent_id,
              delegation_id, is_broadcast, payload_json, timestamp_ms, ttl_ms)
             VALUES (?, ?, ?, ?, ?, NULL, FALSE, ?, ?, NULL)",
        )
        .bind(message_id)
        .bind(&sender.run_id)
        .bind(&sender.agent_id)
        .bind(&receiver.run_id)
        .bind(&receiver.agent_id)
        .bind("{not-valid-json")
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(&pool)
        .await
        .unwrap();

        let receive = tokio::time::timeout(Duration::from_millis(300), stream_b.recv()).await;
        assert!(
            receive.is_err(),
            "invalid direct payload should not be delivered"
        );

        let status = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let row =
                    sqlx::query("SELECT status FROM agent_message_queue WHERE message_id = ?")
                        .bind(message_id)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                let status: String = row.try_get("status").unwrap();
                if status == "failed" {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("invalid direct payload should eventually be dead-lettered");

        assert_eq!(status, "failed");

        cleanup(&pool).await;
    }

    #[tokio::test]
    async fn direct_failure_identity_falls_back_to_message_id() {
        skip_without_db!(pool);

        let transport =
            DatabaseTransport::new(pool.clone()).with_poll_interval(Duration::from_millis(50));

        let sender = addr("run-db-rowid-fallback-a", "alice");
        let receiver = addr("run-db-rowid-fallback-b", "bob");

        transport.register(sender.clone(), None).await.unwrap();
        transport.register(receiver.clone(), None).await.unwrap();

        let msg = Arc::new(AgentMessage::new(
            sender.clone(),
            MessageTarget::Direct {
                address: receiver.clone(),
            },
            MessagePayload::Text {
                content: "fallback".into(),
                summary: None,
            },
        ));
        let message_id = msg.id.clone();
        transport.send(msg).await.unwrap();

        let claimed_by = format!("{}@{}", receiver.run_id, receiver.agent_id);
        sqlx::query(
            "UPDATE agent_message_queue
             SET status = 'claimed', claimed_by = ?, claimed_at_ms = ?
             WHERE message_id = ?",
        )
        .bind(&claimed_by)
        .bind(chrono::Utc::now().timestamp_millis())
        .bind(&message_id)
        .execute(&pool)
        .await
        .unwrap();

        mark_direct_failed_by_identity(&pool, None, Some(&message_id), &claimed_by)
            .await
            .unwrap();

        let row = sqlx::query(
            "SELECT status, claimed_by, claimed_at_ms
             FROM agent_message_queue
             WHERE message_id = ?",
        )
        .bind(&message_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let status: String = row.try_get("status").unwrap();
        let claimed_by_after: Option<String> = row.try_get("claimed_by").unwrap();
        let claimed_at_ms_after: Option<i64> = row.try_get("claimed_at_ms").unwrap();

        assert_eq!(status, "failed");
        assert!(claimed_by_after.is_none());
        assert!(claimed_at_ms_after.is_none());

        cleanup(&pool).await;
    }

    #[tokio::test]
    async fn current_claim_fetch_excludes_preexisting_claimed_rows() {
        skip_without_db!(pool);

        let transport =
            DatabaseTransport::new(pool.clone()).with_poll_interval(Duration::from_millis(50));

        let sender = addr("run-db-claim-scope-a", "alice");
        let receiver = addr("run-db-claim-scope-b", "bob");
        let consumer_id = format!("{}@{}", receiver.agent_id, receiver.run_id);

        transport.register(sender.clone(), None).await.unwrap();
        transport.register(receiver.clone(), None).await.unwrap();

        let mut stream = transport.subscribe(&receiver).await.unwrap();

        let stale = AgentMessage::new(
            sender.clone(),
            MessageTarget::Direct {
                address: receiver.clone(),
            },
            MessagePayload::Text {
                content: "stale-claimed".into(),
                summary: None,
            },
        );

        sqlx::query(
            "INSERT INTO agent_message_queue
             (message_id, from_run_id, from_agent_id, to_run_id, to_agent_id,
              delegation_id, is_broadcast, payload_json, timestamp_ms, ttl_ms,
              status, claimed_by, claimed_at_ms, attempt_count)
             VALUES (?, ?, ?, ?, ?, NULL, FALSE, ?, ?, NULL, 'claimed', ?, ?, 1)",
        )
        .bind(&stale.id)
        .bind(&sender.run_id)
        .bind(&sender.agent_id)
        .bind(&receiver.run_id)
        .bind(&receiver.agent_id)
        .bind(serde_json::to_string(&stale).unwrap())
        .bind(chrono::Utc::now().timestamp_millis() - 5_000)
        .bind(&consumer_id)
        .bind(chrono::Utc::now().timestamp_millis() - 5_000)
        .execute(&pool)
        .await
        .unwrap();

        let fresh = Arc::new(AgentMessage::new(
            sender.clone(),
            MessageTarget::Direct {
                address: receiver.clone(),
            },
            MessagePayload::Text {
                content: "fresh-claim".into(),
                summary: None,
            },
        ));
        transport.send(fresh.clone()).await.unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), stream.recv())
            .await
            .expect("timeout waiting for claimed message")
            .expect("stream closed");

        match &received.payload {
            MessagePayload::Text { content, .. } => {
                assert_eq!(
                    content, "fresh-claim",
                    "poll fetch should only return messages claimed in the current cycle"
                );
            }
            other => panic!("expected Text, got: {other:?}"),
        }

        let extra = tokio::time::timeout(Duration::from_millis(300), stream.recv()).await;
        assert!(
            extra.is_err(),
            "stale preexisting claimed row should not be re-fetched in the same poll cycle"
        );

        cleanup(&pool).await;
    }

    #[tokio::test]
    async fn expired_broadcast_messages_are_marked_failed() {
        skip_without_db!(pool);

        let transport =
            DatabaseTransport::new(pool.clone()).with_poll_interval(Duration::from_millis(50));

        let leader = addr("run-db-bcast-ttl-lead", "leader");
        let worker = addr("run-db-bcast-ttl-worker", "worker");
        let del = "del-db-bcast-ttl";

        transport
            .register(leader.clone(), Some(del.into()))
            .await
            .unwrap();
        transport
            .register(worker.clone(), Some(del.into()))
            .await
            .unwrap();

        let mut stream_worker = transport.subscribe(&worker).await.unwrap();

        let msg = AgentMessage::new(
            leader.clone(),
            MessageTarget::Broadcast {
                delegation_id: del.into(),
            },
            MessagePayload::Text {
                content: "expired-broadcast".into(),
                summary: None,
            },
        )
        .with_ttl(Duration::ZERO);
        let message_id = msg.id.clone();
        transport.broadcast(del, Arc::new(msg)).await.unwrap();

        let receive = tokio::time::timeout(Duration::from_millis(300), stream_worker.recv()).await;
        assert!(
            receive.is_err(),
            "expired broadcast should not be delivered"
        );

        let status = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let row =
                    sqlx::query("SELECT status FROM agent_message_queue WHERE message_id = ?")
                        .bind(&message_id)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                let status: String = row.try_get("status").unwrap();
                if status == "failed" {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("expired broadcast should eventually be dead-lettered");

        assert_eq!(status, "failed");

        cleanup(&pool).await;
    }

    // ── Cleanup Utilities ───────────────────────────────────────────────────

    #[tokio::test]
    async fn cleanup_expired_removes_old_messages() {
        skip_without_db!(pool);

        let transport =
            DatabaseTransport::new(pool.clone()).with_poll_interval(Duration::from_millis(50));

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
        assert!(
            removed >= 1,
            "should have removed at least 1 expired message"
        );

        cleanup(&pool).await;
    }

    #[tokio::test]
    async fn reclaim_stale_requeues_retryable_and_dead_letters_exhausted_messages() {
        skip_without_db!(pool);

        let transport = DatabaseTransport::new(pool.clone())
            .with_poll_interval(Duration::from_millis(50))
            .with_visibility_timeout(Duration::from_millis(1))
            .with_max_delivery_attempts(2);

        let sender = addr("run-db-reclaim-a", "alice");
        let retryable = addr("run-db-reclaim-b", "bob");
        let exhausted = addr("run-db-reclaim-c", "carol");

        transport.register(sender.clone(), None).await.unwrap();
        transport.register(retryable.clone(), None).await.unwrap();
        transport.register(exhausted.clone(), None).await.unwrap();

        let retryable_msg = Arc::new(AgentMessage::new(
            sender.clone(),
            MessageTarget::Direct {
                address: retryable.clone(),
            },
            MessagePayload::Text {
                content: "retry me".into(),
                summary: None,
            },
        ));
        let exhausted_msg = Arc::new(AgentMessage::new(
            sender.clone(),
            MessageTarget::Direct {
                address: exhausted.clone(),
            },
            MessagePayload::Text {
                content: "dead-letter me".into(),
                summary: None,
            },
        ));

        transport.send(retryable_msg.clone()).await.unwrap();
        transport.send(exhausted_msg.clone()).await.unwrap();

        let stale_claimed_at = chrono::Utc::now().timestamp_millis() - 60_000;
        sqlx::query(
            "UPDATE agent_message_queue
             SET status = 'claimed', claimed_by = 'it-consumer', claimed_at_ms = ?, attempt_count = ?
             WHERE message_id = ?",
        )
        .bind(stale_claimed_at)
        .bind(1_i64)
        .bind(&retryable_msg.id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE agent_message_queue
             SET status = 'claimed', claimed_by = 'it-consumer', claimed_at_ms = ?, attempt_count = ?
             WHERE message_id = ?",
        )
        .bind(stale_claimed_at)
        .bind(2_i64)
        .bind(&exhausted_msg.id)
        .execute(&pool)
        .await
        .unwrap();

        let reclaimed = transport.reclaim_stale().await.unwrap();
        assert_eq!(
            reclaimed, 1,
            "only retryable stale messages should be requeued"
        );

        let rows = sqlx::query(
            "SELECT message_id, status, claimed_by, claimed_at_ms
             FROM agent_message_queue
             WHERE message_id IN (?, ?)",
        )
        .bind(&retryable_msg.id)
        .bind(&exhausted_msg.id)
        .fetch_all(&pool)
        .await
        .unwrap();

        let mut by_id = std::collections::HashMap::new();
        for row in rows {
            let message_id: String = row.try_get("message_id").unwrap();
            let status: String = row.try_get("status").unwrap();
            let claimed_by: Option<String> = row.try_get("claimed_by").unwrap();
            let claimed_at_ms: Option<i64> = row.try_get("claimed_at_ms").unwrap();
            by_id.insert(message_id, (status, claimed_by, claimed_at_ms));
        }

        let retryable_state = by_id.get(&retryable_msg.id).expect("retryable message row");
        assert_eq!(retryable_state.0, "pending");
        assert!(retryable_state.1.is_none());
        assert!(retryable_state.2.is_none());

        let exhausted_state = by_id.get(&exhausted_msg.id).expect("exhausted message row");
        assert_eq!(exhausted_state.0, "failed");
        assert!(exhausted_state.1.is_none());
        assert!(exhausted_state.2.is_none());

        cleanup(&pool).await;
    }

    #[tokio::test]
    async fn register_starts_cleanup_scheduler_and_reclaims_stale_claims() {
        skip_without_db!(pool);

        let transport = DatabaseTransport::new(pool.clone())
            .with_visibility_timeout(Duration::from_millis(50))
            .with_max_delivery_attempts(3);

        let sender = addr("run-db-auto-reclaim-a", "alice");
        let receiver = addr("run-db-auto-reclaim-b", "bob");

        transport.register(sender.clone(), None).await.unwrap();
        transport.register(receiver.clone(), None).await.unwrap();

        let msg = Arc::new(AgentMessage::new(
            sender,
            MessageTarget::Direct {
                address: receiver.clone(),
            },
            MessagePayload::Text {
                content: "auto reclaim me".into(),
                summary: None,
            },
        ));
        transport.send(msg.clone()).await.unwrap();

        let stale_claimed_at = chrono::Utc::now().timestamp_millis() - 60_000;
        sqlx::query(
            "UPDATE agent_message_queue
             SET status = 'claimed', claimed_by = 'it-consumer', claimed_at_ms = ?, attempt_count = 1
             WHERE message_id = ?",
        )
        .bind(stale_claimed_at)
        .bind(&msg.id)
        .execute(&pool)
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(180)).await;

        let row = sqlx::query(
            "SELECT status, claimed_by, claimed_at_ms
             FROM agent_message_queue
             WHERE message_id = ?",
        )
        .bind(&msg.id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let status: String = row.try_get("status").unwrap();
        let claimed_by: Option<String> = row.try_get("claimed_by").unwrap();
        let claimed_at_ms: Option<i64> = row.try_get("claimed_at_ms").unwrap();

        assert_eq!(status, "pending");
        assert!(claimed_by.is_none());
        assert!(claimed_at_ms.is_none());

        transport.shutdown().await.unwrap();
        cleanup(&pool).await;
    }

    // ── Unregister stops stream ─────────────────────────────────────────────

    #[tokio::test]
    async fn unregister_stops_poll_task() {
        skip_without_db!(pool);

        let transport =
            DatabaseTransport::new(pool.clone()).with_poll_interval(Duration::from_millis(50));

        let a = addr("run-db-unreg", "agent");
        transport.register(a.clone(), None).await.unwrap();
        assert_eq!(transport.agent_count().await, 1);
        let mut stream = transport.subscribe(&a).await.unwrap();

        transport.unregister(&a).await.unwrap();
        assert_eq!(transport.agent_count().await, 0);
        let closed = tokio::time::timeout(Duration::from_millis(500), stream.recv())
            .await
            .expect("stream should close promptly after unregister");
        assert!(
            closed.is_none(),
            "unregister should terminate the active poll loop and close the stream"
        );

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
