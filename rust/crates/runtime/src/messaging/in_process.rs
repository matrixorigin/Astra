//! In-process transport — tokio channel-based, for CLI and single-process runtimes.
//!
//! Provides microsecond-latency, zero-serialization message delivery using
//! `tokio::sync::mpsc` for direct messages and `tokio::sync::broadcast` for
//! delegation-wide broadcasts.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::{RwLock, broadcast, mpsc};

use super::transport::{MessageStream, MessageTransport};
use super::types::{AgentAddress, AgentMessage, MailboxError};

/// Broadcast channel capacity. Messages beyond this are dropped for slow receivers.
const BROADCAST_CAPACITY: usize = 256;

/// Direct message channel capacity. Provides backpressure under load.
const DIRECT_CHANNEL_CAPACITY: usize = 4096;

// ─── Metrics ────────────────────────────────────────────────────────────────

/// Observable counters for the in-process transport.
#[derive(Debug, Default)]
pub struct InProcessMetrics {
    pub messages_sent: AtomicU64,
    pub messages_received: AtomicU64,
    pub messages_dropped: AtomicU64,
    pub broadcast_lag_events: AtomicU64,
}

// ─── InProcessTransport ─────────────────────────────────────────────────────

/// In-process message transport using tokio channels.
///
/// - **Direct messages**: bounded `mpsc` channel per agent (cap [`DIRECT_CHANNEL_CAPACITY`]).
/// - **Broadcasts**: `broadcast` channel per delegation group.
/// - **Zero serialization**: messages are `Arc<AgentMessage>`, shared by reference.
pub struct InProcessTransport {
    /// Direct message senders keyed by agent address.
    inboxes: RwLock<HashMap<AgentAddress, mpsc::Sender<Arc<AgentMessage>>>>,
    /// Initial direct receivers keyed by agent address until the first subscribe().
    pending_receivers: RwLock<HashMap<AgentAddress, mpsc::Receiver<Arc<AgentMessage>>>>,
    /// Broadcast senders keyed by delegation ID.
    broadcasts: RwLock<HashMap<String, broadcast::Sender<Arc<AgentMessage>>>>,
    /// Maps agent addresses to their delegation group (for broadcast subscription).
    memberships: RwLock<HashMap<AgentAddress, String>>,
    /// Whether shutdown has been called.
    is_shutdown: AtomicBool,
    /// Observable metrics.
    metrics: Arc<InProcessMetrics>,
}

impl InProcessTransport {
    pub fn new() -> Self {
        Self {
            inboxes: RwLock::new(HashMap::new()),
            pending_receivers: RwLock::new(HashMap::new()),
            broadcasts: RwLock::new(HashMap::new()),
            memberships: RwLock::new(HashMap::new()),
            is_shutdown: AtomicBool::new(false),
            metrics: Arc::new(InProcessMetrics::default()),
        }
    }

    /// Get or create a broadcast channel for a delegation group.
    async fn ensure_broadcast(&self, delegation_id: &str) -> broadcast::Sender<Arc<AgentMessage>> {
        let read = self.broadcasts.read().await;
        if let Some(tx) = read.get(delegation_id) {
            return tx.clone();
        }
        drop(read);

        let mut write = self.broadcasts.write().await;
        // Double-check after acquiring write lock.
        write
            .entry(delegation_id.to_string())
            .or_insert_with(|| broadcast::channel(BROADCAST_CAPACITY).0)
            .clone()
    }

    /// Number of currently registered agents (for diagnostics).
    pub async fn agent_count(&self) -> usize {
        self.inboxes.read().await.len()
    }

    /// Get a reference to the transport's metrics counters.
    pub fn metrics(&self) -> &Arc<InProcessMetrics> {
        &self.metrics
    }
}

impl Default for InProcessTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MessageTransport for InProcessTransport {
    async fn register(
        &self,
        addr: AgentAddress,
        delegation_id: Option<String>,
    ) -> Result<(), MailboxError> {
        let (tx, rx) = mpsc::channel(DIRECT_CHANNEL_CAPACITY);
        let mut inboxes = self.inboxes.write().await;
        let mut pending_receivers = self.pending_receivers.write().await;
        pending_receivers.insert(addr.clone(), rx);
        inboxes.insert(addr.clone(), tx);

        if let Some(did) = delegation_id {
            self.ensure_broadcast(&did).await;
            self.memberships.write().await.insert(addr, did);
        }

        Ok(())
    }

    async fn unregister(&self, addr: &AgentAddress) -> Result<(), MailboxError> {
        let mut inboxes = self.inboxes.write().await;
        let mut pending_receivers = self.pending_receivers.write().await;
        inboxes.remove(addr);
        pending_receivers.remove(addr);
        // Extract the delegation_id in a separate statement so the write guard
        // is dropped before we acquire a read lock below.
        let did = self.memberships.write().await.remove(addr);
        if let Some(did) = did {
            // Clean up broadcast channel if no members remain.
            let has_members = self.memberships.read().await.values().any(|d| d == &did);
            if !has_members {
                self.broadcasts.write().await.remove(&did);
            }
        }
        Ok(())
    }

    async fn subscribe(&self, addr: &AgentAddress) -> Result<Box<dyn MessageStream>, MailboxError> {
        // The first subscribe() should attach the receiver created during register()
        // so messages sent during mailbox registration are preserved.
        let mut inboxes = self.inboxes.write().await;
        let mut pending_receivers = self.pending_receivers.write().await;
        let rx = if let Some(rx) = pending_receivers.remove(addr) {
            rx
        } else {
            let (tx, rx) = mpsc::channel(DIRECT_CHANNEL_CAPACITY);
            if !inboxes.contains_key(addr) {
                return Err(MailboxError::AgentNotFound(addr.clone()));
            }
            inboxes.insert(addr.clone(), tx);
            rx
        };

        // Snapshot delegation membership while this subscribe still owns the
        // direct-mailbox state; if the broadcast channel disappears afterward,
        // fail fast instead of silently returning a stream with no broadcasts.
        let delegation_id = self.memberships.read().await.get(addr).cloned();
        drop(pending_receivers);
        drop(inboxes);

        let broadcast_rx = if let Some(did) = delegation_id {
            let broadcasts = self.broadcasts.read().await;
            Some(
                broadcasts
                    .get(&did)
                    .ok_or_else(|| {
                        MailboxError::Transport(format!(
                            "broadcast group not found for registered agent: {did}"
                        ))
                    })?
                    .subscribe(),
            )
        } else {
            None
        };

        Ok(Box::new(InProcessStream {
            direct: rx,
            broadcast: broadcast_rx,
            metrics: Arc::clone(&self.metrics),
        }))
    }

    async fn send(&self, msg: Arc<AgentMessage>) -> Result<(), MailboxError> {
        if self.is_shutdown.load(Ordering::Relaxed) {
            return Err(MailboxError::Transport("transport is shut down".into()));
        }

        let target = match &msg.to {
            super::types::MessageTarget::Direct { address } => address.clone(),
            _ => {
                return Err(MailboxError::Transport(
                    "send() requires Direct target".into(),
                ));
            }
        };

        let inboxes = self.inboxes.read().await;
        let tx = inboxes
            .get(&target)
            .ok_or(MailboxError::AgentNotFound(target))?;
        match tx.try_send(msg) {
            Ok(()) => {
                self.metrics.messages_sent.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics
                    .messages_dropped
                    .fetch_add(1, Ordering::Relaxed);
                Err(MailboxError::Transport(
                    "direct channel full (backpressure)".into(),
                ))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(MailboxError::ChannelClosed),
        }
    }

    async fn broadcast(
        &self,
        delegation_id: &str,
        msg: Arc<AgentMessage>,
    ) -> Result<(), MailboxError> {
        if self.is_shutdown.load(Ordering::Relaxed) {
            return Err(MailboxError::Transport("transport is shut down".into()));
        }

        let broadcasts = self.broadcasts.read().await;
        let Some(tx) = broadcasts.get(delegation_id) else {
            self.metrics
                .messages_dropped
                .fetch_add(1, Ordering::Relaxed);
            return Err(MailboxError::Transport(format!(
                "broadcast group not found: {delegation_id}"
            )));
        };

        match tx.send(msg) {
            Ok(_) => {
                self.metrics.messages_sent.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(_) => {
                self.metrics
                    .messages_dropped
                    .fetch_add(1, Ordering::Relaxed);
                Err(MailboxError::Transport(format!(
                    "broadcast group '{delegation_id}' has no subscribers"
                )))
            }
        }
    }

    async fn shutdown(&self) -> Result<(), MailboxError> {
        self.is_shutdown.store(true, Ordering::Relaxed);
        // Close all direct channels by dropping senders.
        let mut inboxes = self.inboxes.write().await;
        let mut pending_receivers = self.pending_receivers.write().await;
        inboxes.clear();
        pending_receivers.clear();
        // Close all broadcast channels.
        self.broadcasts.write().await.clear();
        Ok(())
    }
}

// ─── InProcessStream ────────────────────────────────────────────────────────

/// Receives both direct and broadcast messages for a single agent.
struct InProcessStream {
    direct: mpsc::Receiver<Arc<AgentMessage>>,
    broadcast: Option<broadcast::Receiver<Arc<AgentMessage>>>,
    metrics: Arc<InProcessMetrics>,
}

#[async_trait]
impl MessageStream for InProcessStream {
    async fn recv(&mut self) -> Option<Arc<AgentMessage>> {
        tokio::select! {
            biased;
            // Prioritize direct messages over broadcasts.
            msg = self.direct.recv() => msg,
            msg = async {
                match self.broadcast.as_mut() {
                    Some(rx) => loop {
                        match rx.recv().await {
                            Ok(m) => break Some(m),
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                let total = self.metrics.broadcast_lag_events.fetch_add(1, Ordering::Relaxed) + 1;
                                eprintln!("  ⚠ messaging: broadcast receiver lagged by {n} messages (total lag events: {total})");
                                continue;
                            }
                            Err(broadcast::error::RecvError::Closed) => break None,
                        }
                    },
                    None => std::future::pending().await,
                }
            } => msg,
        }
    }

    fn try_recv(&mut self) -> Option<Arc<AgentMessage>> {
        // Try direct first.
        if let Ok(msg) = self.direct.try_recv() {
            return Some(msg);
        }
        // Then broadcast (with bounded retry to avoid CPU spin on persistent lag).
        if let Some(ref mut rx) = self.broadcast {
            const MAX_LAG_RETRIES: usize = 64;
            for _ in 0..MAX_LAG_RETRIES {
                match rx.try_recv() {
                    Ok(msg) => return Some(msg),
                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                    _ => return None,
                }
            }
            astra_core::agent_warn!(
                "messaging",
                "broadcast receiver lagged > {MAX_LAG_RETRIES} times — dropping"
            );
        }
        None
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::types::{AgentSignal, MessagePayload, MessageTarget};

    fn addr(run: &str, agent: &str) -> AgentAddress {
        AgentAddress::new(run, agent)
    }

    fn text_msg(from: AgentAddress, to: AgentAddress, content: &str) -> Arc<AgentMessage> {
        Arc::new(AgentMessage::new(
            from,
            MessageTarget::Direct { address: to },
            MessagePayload::Text {
                content: content.into(),
                summary: None,
            },
        ))
    }

    #[tokio::test]
    async fn direct_message_delivery() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "coder");
        let b = addr("r2", "reviewer");

        transport.register(a.clone(), None).await.unwrap();
        transport.register(b.clone(), None).await.unwrap();

        let mut stream_b = transport.subscribe(&b).await.unwrap();

        let msg = text_msg(a.clone(), b.clone(), "please review");
        transport.send(msg).await.unwrap();

        let received = stream_b.try_recv().unwrap();
        match &received.payload {
            MessagePayload::Text { content, .. } => assert_eq!(content, "please review"),
            _ => panic!("expected text payload"),
        }
    }

    #[tokio::test]
    async fn broadcast_delivery() {
        let transport = InProcessTransport::new();
        let leader = addr("r0", "leader");
        let a = addr("r1", "worker-a");
        let b = addr("r2", "worker-b");
        let del_id = "del-1";

        transport
            .register(leader.clone(), Some(del_id.into()))
            .await
            .unwrap();
        transport
            .register(a.clone(), Some(del_id.into()))
            .await
            .unwrap();
        transport
            .register(b.clone(), Some(del_id.into()))
            .await
            .unwrap();

        let mut stream_a = transport.subscribe(&a).await.unwrap();
        let mut stream_b = transport.subscribe(&b).await.unwrap();

        let msg = Arc::new(AgentMessage::new(
            leader.clone(),
            MessageTarget::Broadcast {
                delegation_id: del_id.into(),
            },
            MessagePayload::Signal(AgentSignal::Heartbeat),
        ));
        transport.broadcast(del_id, msg).await.unwrap();

        assert!(stream_a.try_recv().is_some());
        assert!(stream_b.try_recv().is_some());
    }

    #[tokio::test]
    async fn unregister_cleans_up() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");

        transport.register(a.clone(), None).await.unwrap();
        assert_eq!(transport.agent_count().await, 1);

        transport.unregister(&a).await.unwrap();
        assert_eq!(transport.agent_count().await, 0);

        // Sending to unregistered agent fails.
        let msg = text_msg(addr("r0", "x"), a.clone(), "hello");
        assert!(transport.send(msg).await.is_err());
    }

    #[tokio::test]
    async fn subscribe_requires_registration() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        assert!(transport.subscribe(&a).await.is_err());
    }

    #[tokio::test]
    async fn drain_returns_all_buffered() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        let b = addr("r2", "b");

        transport.register(a.clone(), None).await.unwrap();
        transport.register(b.clone(), None).await.unwrap();

        let mut stream_b = transport.subscribe(&b).await.unwrap();

        for i in 0..5 {
            let msg = text_msg(a.clone(), b.clone(), &format!("msg-{i}"));
            transport.send(msg).await.unwrap();
        }

        let drained = stream_b.drain();
        assert_eq!(drained.len(), 5);
    }

    #[tokio::test]
    async fn broadcast_cleanup_on_last_unregister() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        let b = addr("r2", "b");
        let del = "del-x";

        transport
            .register(a.clone(), Some(del.into()))
            .await
            .unwrap();
        transport
            .register(b.clone(), Some(del.into()))
            .await
            .unwrap();

        // Unregister one — broadcast should remain.
        transport.unregister(&a).await.unwrap();
        assert!(transport.broadcasts.read().await.contains_key(del));

        // Unregister last — broadcast should be cleaned up.
        transport.unregister(&b).await.unwrap();
        assert!(!transport.broadcasts.read().await.contains_key(del));
    }

    #[tokio::test]
    async fn subscribe_fails_when_membership_loses_broadcast_channel() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        let del = "del-missing";

        transport
            .register(a.clone(), Some(del.into()))
            .await
            .unwrap();
        transport.broadcasts.write().await.remove(del);

        let err = match transport.subscribe(&a).await {
            Ok(_) => panic!("subscribe should fail when broadcast channel is missing"),
            Err(err) => err,
        };
        assert!(matches!(err, MailboxError::Transport(_)));
    }

    #[tokio::test]
    async fn shutdown_prevents_new_sends() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        let b = addr("r2", "b");

        transport.register(a.clone(), None).await.unwrap();
        transport.register(b.clone(), None).await.unwrap();

        transport.shutdown().await.unwrap();

        let msg = text_msg(a.clone(), b.clone(), "after shutdown");
        assert!(transport.send(msg).await.is_err());
    }

    #[tokio::test]
    async fn shutdown_prevents_new_broadcasts() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");

        transport
            .register(a.clone(), Some("del".into()))
            .await
            .unwrap();
        transport.shutdown().await.unwrap();

        let msg = Arc::new(AgentMessage::new(
            a.clone(),
            MessageTarget::Broadcast {
                delegation_id: "del".into(),
            },
            MessagePayload::Signal(AgentSignal::Heartbeat),
        ));
        assert!(transport.broadcast("del", msg).await.is_err());
    }

    #[tokio::test]
    async fn metrics_track_send_count() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        let b = addr("r2", "b");

        transport.register(a.clone(), None).await.unwrap();
        transport.register(b.clone(), None).await.unwrap();
        let _stream = transport.subscribe(&b).await.unwrap();

        for i in 0..3 {
            let msg = text_msg(a.clone(), b.clone(), &format!("m{i}"));
            transport.send(msg).await.unwrap();
        }

        assert_eq!(transport.metrics().messages_sent.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn metrics_track_dropped_on_backpressure() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        let b = addr("r2", "b");

        transport.register(a.clone(), None).await.unwrap();
        transport.register(b.clone(), None).await.unwrap();
        // Subscribe creates a bounded channel (cap 4096).
        let _stream = transport.subscribe(&b).await.unwrap();

        // Fill the channel beyond capacity.
        let mut sent = 0u64;
        let mut dropped = 0u64;
        for i in 0..5000 {
            let msg = text_msg(a.clone(), b.clone(), &format!("flood-{i}"));
            match transport.send(msg).await {
                Ok(()) => sent += 1,
                Err(_) => dropped += 1,
            }
        }

        assert_eq!(
            transport.metrics().messages_sent.load(Ordering::Relaxed),
            sent
        );
        assert_eq!(
            transport.metrics().messages_dropped.load(Ordering::Relaxed),
            dropped
        );
        assert!(
            dropped > 0,
            "should have dropped some messages due to backpressure"
        );
    }

    #[tokio::test]
    async fn broadcast_without_registered_group_returns_error() {
        let transport = InProcessTransport::new();
        let msg = Arc::new(AgentMessage::new(
            addr("r0", "sender"),
            MessageTarget::Broadcast {
                delegation_id: "missing-del".into(),
            },
            MessagePayload::Signal(AgentSignal::Heartbeat),
        ));

        let err = transport
            .broadcast("missing-del", msg)
            .await
            .expect_err("missing broadcast group should error");
        match err {
            MailboxError::Transport(message) => {
                assert!(message.contains("broadcast group not found"));
            }
            other => panic!("expected transport error, got {other:?}"),
        }
        assert_eq!(
            transport.metrics().messages_dropped.load(Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn broadcast_without_subscribers_returns_error() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");

        transport
            .register(a.clone(), Some("del-empty".into()))
            .await
            .unwrap();

        let msg = Arc::new(AgentMessage::new(
            a.clone(),
            MessageTarget::Broadcast {
                delegation_id: "del-empty".into(),
            },
            MessagePayload::Signal(AgentSignal::Heartbeat),
        ));

        let err = transport
            .broadcast("del-empty", msg)
            .await
            .expect_err("broadcast without subscribers should error");
        match err {
            MailboxError::Transport(message) => {
                assert!(message.contains("has no subscribers"));
            }
            other => panic!("expected transport error, got {other:?}"),
        }
        assert_eq!(
            transport.metrics().messages_dropped.load(Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn send_before_initial_subscribe_is_buffered() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        let b = addr("r2", "b");

        transport.register(a.clone(), None).await.unwrap();
        transport.register(b.clone(), None).await.unwrap();

        let msg = text_msg(a.clone(), b.clone(), "queued before subscribe");
        transport.send(msg).await.unwrap();

        let mut stream_b = transport.subscribe(&b).await.unwrap();
        let received = stream_b
            .try_recv()
            .expect("message queued before subscribe");
        match &received.payload {
            MessagePayload::Text { content, .. } => assert_eq!(content, "queued before subscribe"),
            other => panic!("expected text payload, got {other:?}"),
        }
    }
}
