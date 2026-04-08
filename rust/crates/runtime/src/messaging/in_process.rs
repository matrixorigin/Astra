//! In-process transport — tokio channel-based, for CLI and single-process runtimes.
//!
//! Provides microsecond-latency, zero-serialization message delivery using
//! `tokio::sync::mpsc` for direct messages and `tokio::sync::broadcast` for
//! delegation-wide broadcasts.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc, RwLock};

use super::transport::{MessageStream, MessageTransport};
use super::types::{AgentAddress, AgentMessage, MailboxError};

/// Broadcast channel capacity. Messages beyond this are dropped for slow receivers.
const BROADCAST_CAPACITY: usize = 256;

/// Direct message channel capacity. Provides backpressure under load.
const DIRECT_CHANNEL_CAPACITY: usize = 4096;

// ─── InProcessTransport ─────────────────────────────────────────────────────

/// In-process message transport using tokio channels.
///
/// - **Direct messages**: bounded `mpsc` channel per agent (cap [`DIRECT_CHANNEL_CAPACITY`]).
/// - **Broadcasts**: `broadcast` channel per delegation group.
/// - **Zero serialization**: messages are `Arc<AgentMessage>`, shared by reference.
pub struct InProcessTransport {
    /// Direct message senders keyed by agent address.
    inboxes: RwLock<HashMap<AgentAddress, mpsc::Sender<Arc<AgentMessage>>>>,
    /// Broadcast senders keyed by delegation ID.
    broadcasts: RwLock<HashMap<String, broadcast::Sender<Arc<AgentMessage>>>>,
    /// Maps agent addresses to their delegation group (for broadcast subscription).
    memberships: RwLock<HashMap<AgentAddress, String>>,
}

impl InProcessTransport {
    pub fn new() -> Self {
        Self {
            inboxes: RwLock::new(HashMap::new()),
            broadcasts: RwLock::new(HashMap::new()),
            memberships: RwLock::new(HashMap::new()),
        }
    }

    /// Get or create a broadcast channel for a delegation group.
    async fn ensure_broadcast(
        &self,
        delegation_id: &str,
    ) -> broadcast::Sender<Arc<AgentMessage>> {
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
        let (tx, _rx) = mpsc::channel(DIRECT_CHANNEL_CAPACITY);

        // Store the sender; subscribe() creates a fresh channel and replaces it.

        self.inboxes.write().await.insert(addr.clone(), tx);

        if let Some(did) = delegation_id {
            self.ensure_broadcast(&did).await;
            self.memberships.write().await.insert(addr, did);
        }

        Ok(())
    }

    async fn unregister(&self, addr: &AgentAddress) -> Result<(), MailboxError> {
        self.inboxes.write().await.remove(addr);
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

    async fn subscribe(
        &self,
        addr: &AgentAddress,
    ) -> Result<Box<dyn MessageStream>, MailboxError> {
        // Create a fresh channel and replace the sender in inboxes.
        let (tx, rx) = mpsc::channel(DIRECT_CHANNEL_CAPACITY);
        {
            let mut inboxes = self.inboxes.write().await;
            if !inboxes.contains_key(addr) {
                return Err(MailboxError::AgentNotFound(addr.clone()));
            }
            inboxes.insert(addr.clone(), tx);
        }

        // Subscribe to broadcast if agent is in a delegation group.
        let broadcast_rx = {
            let memberships = self.memberships.read().await;
            if let Some(did) = memberships.get(addr) {
                let broadcasts = self.broadcasts.read().await;
                broadcasts.get(did).map(|tx| tx.subscribe())
            } else {
                None
            }
        };

        Ok(Box::new(InProcessStream {
            direct: rx,
            broadcast: broadcast_rx,
        }))
    }

    async fn send(&self, msg: Arc<AgentMessage>) -> Result<(), MailboxError> {
        let target = match &msg.to {
            super::types::MessageTarget::Direct { address } => address.clone(),
            _ => return Err(MailboxError::Transport("send() requires Direct target".into())),
        };

        let inboxes = self.inboxes.read().await;
        let tx = inboxes
            .get(&target)
            .ok_or_else(|| MailboxError::AgentNotFound(target))?;
        tx.try_send(msg).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => {
                MailboxError::Transport("direct channel full (backpressure)".into())
            }
            mpsc::error::TrySendError::Closed(_) => MailboxError::ChannelClosed,
        })
    }

    async fn broadcast(
        &self,
        delegation_id: &str,
        msg: Arc<AgentMessage>,
    ) -> Result<(), MailboxError> {
        let broadcasts = self.broadcasts.read().await;
        if let Some(tx) = broadcasts.get(delegation_id) {
            // broadcast::send returns Err only if there are 0 receivers, which is fine.
            let _ = tx.send(msg);
        }
        Ok(())
    }
}

// ─── InProcessStream ────────────────────────────────────────────────────────

/// Receives both direct and broadcast messages for a single agent.
struct InProcessStream {
    direct: mpsc::Receiver<Arc<AgentMessage>>,
    broadcast: Option<broadcast::Receiver<Arc<AgentMessage>>>,
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
                                eprintln!("  ⚠ messaging: broadcast receiver lagged by {n} messages (some dropped)");
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
        // Then broadcast.
        if let Some(ref mut rx) = self.broadcast {
            loop {
                match rx.try_recv() {
                    Ok(msg) => return Some(msg),
                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                    _ => return None,
                }
            }
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
            MessageTarget::Direct {
                address: to,
            },
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

        transport.register(leader.clone(), Some(del_id.into())).await.unwrap();
        transport.register(a.clone(), Some(del_id.into())).await.unwrap();
        transport.register(b.clone(), Some(del_id.into())).await.unwrap();

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

        transport.register(a.clone(), Some(del.into())).await.unwrap();
        transport.register(b.clone(), Some(del.into())).await.unwrap();

        // Unregister one — broadcast should remain.
        transport.unregister(&a).await.unwrap();
        assert!(transport.broadcasts.read().await.contains_key(del));

        // Unregister last — broadcast should be cleaned up.
        transport.unregister(&b).await.unwrap();
        assert!(!transport.broadcasts.read().await.contains_key(del));
    }
}
