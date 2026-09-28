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

use super::transport::{MailboxSubscription, MessageStream, MessageTransport};
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

type Inboxes = HashMap<AgentAddress, (MailboxSubscription, mpsc::Sender<Arc<AgentMessage>>)>;

/// In-process message transport using tokio channels.
///
/// - **Direct messages**: bounded `mpsc` channel per agent (cap [`DIRECT_CHANNEL_CAPACITY`]).
/// - **Broadcasts**: `broadcast` channel per delegation group.
/// - **Zero serialization**: messages are `Arc<AgentMessage>`, shared by reference.
pub struct InProcessTransport {
    /// Direct message senders keyed by agent address.
    inboxes: RwLock<Inboxes>,
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
    ) -> Result<MailboxSubscription, MailboxError> {
        if self.is_shutdown.load(Ordering::Relaxed) {
            return Err(MailboxError::Transport("transport is shut down".into()));
        }
        let (tx, rx) = mpsc::channel(DIRECT_CHANNEL_CAPACITY);
        let subscription = MailboxSubscription::new(addr.clone());
        let mut inboxes = self.inboxes.write().await;
        let mut pending_receivers = self.pending_receivers.write().await;
        let mut memberships = self.memberships.write().await;
        let previous_delegation = memberships.get(&addr).cloned();
        let mut broadcasts = if delegation_id.is_some() || previous_delegation.is_some() {
            Some(self.broadcasts.write().await)
        } else {
            None
        };
        if let (Some(did), Some(broadcasts)) = (delegation_id.as_deref(), broadcasts.as_mut()) {
            broadcasts
                .entry(did.to_string())
                .or_insert_with(|| broadcast::channel(BROADCAST_CAPACITY).0);
        }
        // No await after publication begins: cancellation cannot leave an
        // inbox, receiver, or delegation membership only partly installed.
        pending_receivers.insert(addr.clone(), rx);
        inboxes.insert(addr.clone(), (subscription.clone(), tx));
        if let Some(did) = delegation_id {
            memberships.insert(addr, did);
        } else {
            memberships.remove(&addr);
        }
        if let (Some(previous), Some(broadcasts)) = (previous_delegation, broadcasts.as_mut())
            && !memberships.values().any(|did| did == &previous)
        {
            broadcasts.remove(&previous);
        }

        Ok(subscription)
    }

    async fn unregister(&self, subscription: &MailboxSubscription) -> Result<(), MailboxError> {
        let addr = subscription.address();
        let mut inboxes = self.inboxes.write().await;
        if !inboxes
            .get(addr)
            .is_some_and(|(current, _)| current == subscription)
        {
            return Ok(());
        }
        let mut pending_receivers = self.pending_receivers.write().await;
        let mut memberships = self.memberships.write().await;
        let mut broadcasts = self.broadcasts.write().await;
        // No await after removing the inbox: cancellation cannot leave a
        // routable group membership pointing to a closed direct mailbox.
        inboxes.remove(addr);
        pending_receivers.remove(addr);
        let did = memberships.remove(addr);
        if let Some(did) = did {
            // Clean up broadcast channel if no members remain.
            let has_members = memberships.values().any(|d| d == &did);
            if !has_members {
                broadcasts.remove(&did);
            }
        }
        Ok(())
    }

    async fn subscribe(
        &self,
        subscription: &MailboxSubscription,
    ) -> Result<Box<dyn MessageStream>, MailboxError> {
        let addr = subscription.address();
        if self.is_shutdown.load(Ordering::Relaxed) {
            return Err(MailboxError::Transport("transport is shut down".into()));
        }
        let delegation_id = self.memberships.read().await.get(addr).cloned();
        let broadcast_rx = if let Some(did) = delegation_id {
            Some(self.ensure_broadcast(&did).await.subscribe())
        } else {
            None
        };

        // All awaits that can fail or be cancelled precede receiver transfer.
        // A replacement/unregister between the membership snapshot and here is
        // fenced by the subscription check while the direct-mailbox locks are held.
        let inboxes = self.inboxes.write().await;
        if !inboxes
            .get(addr)
            .is_some_and(|(current, _)| current == subscription)
        {
            return Err(MailboxError::AgentNotFound(addr.clone()));
        }
        let mut pending_receivers = self.pending_receivers.write().await;
        let rx = pending_receivers.remove(addr).ok_or_else(|| {
            MailboxError::Protocol("subscription already attached; register a replacement".into())
        })?;

        Ok(Box::new(InProcessStream {
            direct: rx,
            broadcast: broadcast_rx,
            metrics: Arc::clone(&self.metrics),
        }))
    }

    async fn resolve_agent(
        &self,
        delegation_id: &str,
        agent_id: &str,
    ) -> Result<AgentAddress, MailboxError> {
        self.memberships
            .read()
            .await
            .iter()
            .find_map(|(address, member_delegation)| {
                (member_delegation == delegation_id && address.agent_id == agent_id)
                    .then(|| address.clone())
            })
            .ok_or_else(|| MailboxError::AgentNotFound(AgentAddress::new("", agent_id)))
    }

    async fn list_agents(&self, delegation_id: &str) -> Result<Vec<AgentAddress>, MailboxError> {
        let mut agents = self
            .memberships
            .read()
            .await
            .iter()
            .filter(|(_, member_delegation)| *member_delegation == delegation_id)
            .map(|(address, _)| address.clone())
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| {
            left.agent_id
                .cmp(&right.agent_id)
                .then_with(|| left.run_id.cmp(&right.run_id))
        });
        Ok(agents)
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
        let (_, tx) = inboxes
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
            tracing::warn!(
                target: "astra_messaging",
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
    use crate::types::{AgentSignal, MessagePayload, MessageTarget};
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    fn poll_once_pending(future: std::pin::Pin<&mut impl Future>) {
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(future.poll(&mut context), Poll::Pending));
    }

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

        let _a_subscription = transport.register(a.clone(), None).await.unwrap();
        let _b_subscription = transport.register(b.clone(), None).await.unwrap();

        let mut stream_b = transport.subscribe(&_b_subscription).await.unwrap();

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

        let _leader_subscription = transport
            .register(leader.clone(), Some(del_id.into()))
            .await
            .unwrap();
        let _a_subscription = transport
            .register(a.clone(), Some(del_id.into()))
            .await
            .unwrap();
        let _b_subscription = transport
            .register(b.clone(), Some(del_id.into()))
            .await
            .unwrap();

        let mut stream_a = transport.subscribe(&_a_subscription).await.unwrap();
        let mut stream_b = transport.subscribe(&_b_subscription).await.unwrap();

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

        let _a_subscription = transport.register(a.clone(), None).await.unwrap();
        assert_eq!(transport.agent_count().await, 1);

        transport.unregister(&_a_subscription).await.unwrap();
        assert_eq!(transport.agent_count().await, 0);

        // Sending to unregistered agent fails.
        let msg = text_msg(addr("r0", "x"), a.clone(), "hello");
        assert!(transport.send(msg).await.is_err());
    }

    #[tokio::test]
    async fn stale_subscription_cannot_attach_or_unregister_replacement() {
        let transport = InProcessTransport::new();
        let address = addr("reused-run", "agent");
        let old = transport.register(address.clone(), None).await.unwrap();
        let _old_stream = transport.subscribe(&old).await.unwrap();
        assert!(
            transport.subscribe(&old).await.is_err(),
            "registration attaches only once"
        );
        let new = transport.register(address.clone(), None).await.unwrap();
        let mut stream = transport.subscribe(&new).await.unwrap();
        transport.unregister(&old).await.unwrap();
        assert!(transport.subscribe(&old).await.is_err());
        let message = text_msg(addr("sender", "sender"), address, "new owner");
        let id = message.id.clone();
        transport.send(message).await.unwrap();
        assert_eq!(stream.try_recv().unwrap().id, id);
        transport.unregister(&new).await.unwrap();
        assert_eq!(transport.agent_count().await, 0);
    }

    #[tokio::test]
    async fn cancelled_register_does_not_publish_partial_mailbox() {
        let transport = InProcessTransport::new();
        let address = addr("run", "agent");
        let memberships = transport.memberships.write().await;
        let mut registration = Box::pin(transport.register(address.clone(), None));
        poll_once_pending(registration.as_mut());
        drop(registration);
        drop(memberships);

        assert_eq!(transport.agent_count().await, 0);
        let subscription = transport.register(address.clone(), None).await.unwrap();
        assert!(transport.subscribe(&subscription).await.is_ok());
    }

    #[tokio::test]
    async fn cancelled_subscribe_does_not_take_buffered_receiver() {
        let transport = InProcessTransport::new();
        let address = addr("run", "agent");
        let subscription = transport.register(address.clone(), None).await.unwrap();
        let message = text_msg(addr("sender", "sender"), address, "still buffered");
        let id = message.id.clone();
        transport.send(message).await.unwrap();

        let memberships = transport.memberships.write().await;
        let mut attach = Box::pin(transport.subscribe(&subscription));
        poll_once_pending(attach.as_mut());
        drop(attach);
        drop(memberships);

        let mut stream = transport.subscribe(&subscription).await.unwrap();
        assert_eq!(stream.try_recv().unwrap().id, id);
    }

    #[tokio::test]
    async fn cancelled_unregister_keeps_direct_and_group_routes_together() {
        let transport = InProcessTransport::new();
        let address = addr("run", "agent");
        let subscription = transport
            .register(address.clone(), Some("delegation".into()))
            .await
            .unwrap();
        let broadcasts = transport.broadcasts.write().await;
        let mut cleanup = Box::pin(transport.unregister(&subscription));
        poll_once_pending(cleanup.as_mut());
        drop(cleanup);
        drop(broadcasts);

        assert_eq!(transport.agent_count().await, 1);
        assert_eq!(
            transport
                .resolve_agent("delegation", "agent")
                .await
                .unwrap(),
            address
        );
        transport.unregister(&subscription).await.unwrap();
        assert_eq!(transport.agent_count().await, 0);
    }

    #[tokio::test]
    async fn subscribe_requires_registration() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        assert!(
            transport
                .subscribe(&MailboxSubscription::new(a))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn drain_returns_all_buffered() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        let b = addr("r2", "b");

        let _a_subscription = transport.register(a.clone(), None).await.unwrap();
        let _b_subscription = transport.register(b.clone(), None).await.unwrap();

        let mut stream_b = transport.subscribe(&_b_subscription).await.unwrap();

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

        let _a_subscription = transport
            .register(a.clone(), Some(del.into()))
            .await
            .unwrap();
        let _b_subscription = transport
            .register(b.clone(), Some(del.into()))
            .await
            .unwrap();

        // Unregister one — broadcast should remain.
        transport.unregister(&_a_subscription).await.unwrap();
        assert!(transport.broadcasts.read().await.contains_key(del));

        // Unregister last — broadcast should be cleaned up.
        transport.unregister(&_b_subscription).await.unwrap();
        assert!(!transport.broadcasts.read().await.contains_key(del));
    }

    #[tokio::test]
    async fn subscribe_recreates_missing_broadcast_channel_for_registered_agent() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        let del = "del-missing";

        let _a_subscription = transport
            .register(a.clone(), Some(del.into()))
            .await
            .unwrap();
        transport.broadcasts.write().await.remove(del);

        let _stream = transport
            .subscribe(&_a_subscription)
            .await
            .expect("subscribe should heal a missing broadcast channel");
        assert!(transport.broadcasts.read().await.contains_key(del));
    }

    #[tokio::test]
    async fn shutdown_prevents_new_sends() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        let b = addr("r2", "b");

        let _a_subscription = transport.register(a.clone(), None).await.unwrap();
        let _b_subscription = transport.register(b.clone(), None).await.unwrap();

        transport.shutdown().await.unwrap();

        let msg = text_msg(a.clone(), b.clone(), "after shutdown");
        assert!(transport.send(msg).await.is_err());
    }

    #[tokio::test]
    async fn shutdown_prevents_new_broadcasts() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");

        let _a_subscription = transport
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
    async fn shutdown_prevents_new_registrations() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");

        transport.shutdown().await.unwrap();

        let err = transport.register(a, None).await.unwrap_err();
        assert!(matches!(err, MailboxError::Transport(_)));
        assert_eq!(transport.agent_count().await, 0);
    }

    #[tokio::test]
    async fn metrics_track_send_count() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        let b = addr("r2", "b");

        let _a_subscription = transport.register(a.clone(), None).await.unwrap();
        let _b_subscription = transport.register(b.clone(), None).await.unwrap();
        let _stream = transport.subscribe(&_b_subscription).await.unwrap();

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

        let _a_subscription = transport.register(a.clone(), None).await.unwrap();
        let _b_subscription = transport.register(b.clone(), None).await.unwrap();
        // Subscribe creates a bounded channel (cap 4096).
        let _stream = transport.subscribe(&_b_subscription).await.unwrap();

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

        let _a_subscription = transport
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

        let _a_subscription = transport.register(a.clone(), None).await.unwrap();
        let _b_subscription = transport.register(b.clone(), None).await.unwrap();

        let msg = text_msg(a.clone(), b.clone(), "queued before subscribe");
        transport.send(msg).await.unwrap();

        let mut stream_b = transport.subscribe(&_b_subscription).await.unwrap();
        let received = stream_b
            .try_recv()
            .expect("message queued before subscribe");
        match &received.payload {
            MessagePayload::Text { content, .. } => assert_eq!(content, "queued before subscribe"),
            other => panic!("expected text payload, got {other:?}"),
        }
    }
}
