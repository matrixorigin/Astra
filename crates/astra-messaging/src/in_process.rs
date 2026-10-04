//! In-process transport for CLI and single-process runtimes.
//!
//! Provides in-process message delivery using retained direct inboxes and
//! best-effort delegation broadcasts.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::{Notify, RwLock, broadcast};

use super::transport::{MailboxSubscription, MessageStream, MessageTransport};
use super::types::{AgentAddress, AgentMessage, MailboxError};

/// Broadcast channel capacity. Messages beyond this are dropped for slow receivers.
const BROADCAST_CAPACITY: usize = 256;

/// Direct messages remain charged until the runtime confirms consumption.
const DIRECT_INBOX_CAPACITY: usize = 4096;
const MAX_RETAINED_INBOXES: usize = 8192;
const MAX_DIRECT_MESSAGE_BYTES: usize = 128 * 1024;
const MAX_DIRECT_INBOX_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOTAL_DIRECT_BYTES: usize = 64 * 1024 * 1024;
const DIRECT_ENVELOPE_OVERHEAD: usize = 256;

#[derive(Default)]
struct ByteCounter(usize);

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "message size overflow")
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct DirectEnvelope {
    message: Arc<AgentMessage>,
    charged_bytes: usize,
}

// ─── InProcessTransport ─────────────────────────────────────────────────────

type Inboxes = HashMap<AgentAddress, Arc<DirectInbox>>;

#[derive(Default)]
struct DirectInboxState {
    subscription: Option<MailboxSubscription>,
    last_owner: Option<MailboxSubscription>,
    attached: bool,
    pending: VecDeque<DirectEnvelope>,
    outstanding: usize,
    outstanding_bytes: usize,
    retired: bool,
}

#[derive(Default)]
struct DirectInbox {
    state: Mutex<DirectInboxState>,
    ready: Notify,
}

impl DirectInbox {
    fn state(&self) -> std::sync::MutexGuard<'_, DirectInboxState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// In-process message transport with retained direct inboxes.
///
/// - **Direct messages**: bounded per-address inbox, including unacknowledged deliveries.
/// - **Broadcasts**: `broadcast` channel per delegation group.
/// - **Shared payloads**: messages are `Arc<AgentMessage>`; size accounting
///   serializes to a counting writer without allocating a second payload.
pub struct InProcessTransport {
    /// Direct inboxes survive subscriber detach across turns.
    inboxes: RwLock<Inboxes>,
    /// Broadcast senders keyed by delegation ID.
    broadcasts: RwLock<HashMap<String, broadcast::Sender<Arc<AgentMessage>>>>,
    /// Maps agent addresses to their delegation group (for broadcast subscription).
    memberships: RwLock<HashMap<AgentAddress, String>>,
    /// Whether shutdown has been called.
    is_shutdown: AtomicBool,
    /// Total retained direct-message charge, including unacknowledged deliveries.
    total_direct_bytes: Arc<AtomicUsize>,
}

impl InProcessTransport {
    pub fn new() -> Self {
        Self {
            inboxes: RwLock::new(HashMap::new()),
            broadcasts: RwLock::new(HashMap::new()),
            memberships: RwLock::new(HashMap::new()),
            is_shutdown: AtomicBool::new(false),
            total_direct_bytes: Arc::new(AtomicUsize::new(0)),
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
        self.inboxes
            .read()
            .await
            .values()
            .filter(|inbox| inbox.state().subscription.is_some())
            .count()
    }

    /// Includes detached inboxes, which still consume the address budget.
    pub async fn retained_inbox_count(&self) -> usize {
        self.inboxes.read().await.len()
    }

    async fn enqueue(&self, msg: Arc<AgentMessage>) -> Result<(), MailboxError> {
        if self.is_shutdown.load(Ordering::Relaxed) {
            return Err(MailboxError::DeliveryRejected(
                "transport is shut down".into(),
            ));
        }
        let target = match &msg.to {
            super::types::MessageTarget::Direct { address } => address.clone(),
            _ => {
                return Err(MailboxError::DeliveryRejected(
                    "send requires Direct target".into(),
                ));
            }
        };
        let mut counter = ByteCounter::default();
        serde_json::to_writer(&mut counter, &msg)
            .map_err(|error| MailboxError::DeliveryRejected(format!("message size: {error}")))?;
        let charge = counter.0.saturating_add(DIRECT_ENVELOPE_OVERHEAD);
        if charge > MAX_DIRECT_MESSAGE_BYTES {
            return Err(MailboxError::DeliveryRejected(
                "direct message too large".into(),
            ));
        }
        let inboxes = self.inboxes.read().await;
        let inbox = inboxes
            .get(&target)
            .ok_or_else(|| MailboxError::AgentNotFound(target.clone()))?;
        let mut state = inbox.state();
        if state.outstanding >= DIRECT_INBOX_CAPACITY
            || state.outstanding_bytes.saturating_add(charge) > MAX_DIRECT_INBOX_BYTES
        {
            return Err(MailboxError::DeliveryRejected(
                "direct inbox full (backpressure)".into(),
            ));
        }
        if self
            .total_direct_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(charge)
                    .filter(|next| *next <= MAX_TOTAL_DIRECT_BYTES)
            })
            .is_err()
        {
            return Err(MailboxError::DeliveryRejected(
                "direct transport byte capacity reached".into(),
            ));
        }
        state.pending.push_back(DirectEnvelope {
            message: msg,
            charged_bytes: charge,
        });
        state.outstanding += 1;
        state.outstanding_bytes += charge;
        inbox.ready.notify_one();
        Ok(())
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
        let subscription = MailboxSubscription::new(addr.clone());
        let mut inboxes = self.inboxes.write().await;
        let mut memberships = self.memberships.write().await;
        if let Some(inbox) = inboxes.get(&addr) {
            let state = inbox.state();
            if state.attached {
                return Err(MailboxError::Protocol(format!(
                    "mailbox {addr} already has an owner"
                )));
            }
        } else if inboxes.len() >= MAX_RETAINED_INBOXES {
            return Err(MailboxError::Transport(
                "retained mailbox address capacity reached".into(),
            ));
        }
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
        let inbox = Arc::clone(
            inboxes
                .entry(addr.clone())
                .or_insert_with(|| Arc::new(DirectInbox::default())),
        );
        {
            let mut state = inbox.state();
            // No await after publication begins: the address and membership
            // become visible together, and a new stream reuses the same queue.
            state.subscription = Some(subscription.clone());
            state.last_owner = Some(subscription.clone());
        }
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
        let inboxes = self.inboxes.write().await;
        let Some(inbox) = inboxes.get(addr) else {
            return Ok(());
        };
        let mut memberships = self.memberships.write().await;
        let mut broadcasts = self.broadcasts.write().await;
        let mut state = inbox.state();
        if state.subscription.as_ref() != Some(subscription) {
            return Ok(());
        }
        // Retain the inbox, including accepted deliveries, until a later
        // registration attaches to the same canonical address.
        state.subscription = None;
        inbox.ready.notify_waiters();
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

    async fn retire(&self, subscription: &MailboxSubscription) -> Result<(), MailboxError> {
        let mut inboxes = self.inboxes.write().await;
        let Some(inbox) = inboxes.get(subscription.address()) else {
            return Ok(());
        };
        let mut state = inbox.state();
        if state.last_owner.as_ref() != Some(subscription) || state.subscription.is_some() {
            return Ok(());
        }
        let abandoned_bytes = state.outstanding_bytes;
        state.retired = true;
        state.pending.clear();
        state.outstanding = 0;
        state.outstanding_bytes = 0;
        drop(state);
        inboxes.remove(subscription.address());
        self.total_direct_bytes
            .fetch_sub(abandoned_bytes, Ordering::AcqRel);
        Ok(())
    }

    async fn forget_abandoned_route(
        &self,
        subscription: &MailboxSubscription,
    ) -> Result<bool, MailboxError> {
        // A sender holds the read side of this lock through enqueue, so the
        // empty check and removal cannot race an accepted direct message.
        let mut inboxes = self.inboxes.write().await;
        let Some(inbox) = inboxes.get(subscription.address()) else {
            return Ok(false);
        };
        let mut state = inbox.state();
        if state.last_owner.as_ref() != Some(subscription)
            || state.subscription.is_some()
            || state.attached
            || state.outstanding != 0
            || !state.pending.is_empty()
        {
            return Ok(false);
        }
        state.retired = true;
        drop(state);
        inboxes.remove(subscription.address());
        Ok(true)
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
        let inboxes = self.inboxes.read().await;
        let inbox = inboxes
            .get(addr)
            .ok_or_else(|| MailboxError::AgentNotFound(addr.clone()))?
            .clone();
        {
            let mut state = inbox.state();
            if state.subscription.as_ref() != Some(subscription) {
                return Err(MailboxError::AgentNotFound(addr.clone()));
            }
            if state.attached {
                return Err(MailboxError::Protocol(
                    "subscription already attached".into(),
                ));
            }
            state.attached = true;
        }

        Ok(Box::new(InProcessStream {
            direct: inbox,
            subscription: subscription.clone(),
            delivered: VecDeque::new(),
            detached: false,
            broadcast: broadcast_rx,
            total_direct_bytes: Arc::clone(&self.total_direct_bytes),
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
        self.enqueue(msg).await
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
            return Err(MailboxError::Transport(format!(
                "broadcast group not found: {delegation_id}"
            )));
        };

        match tx.send(msg) {
            Ok(_) => Ok(()),
            Err(_) => Err(MailboxError::Transport(format!(
                "broadcast group '{delegation_id}' has no subscribers"
            ))),
        }
    }

    async fn shutdown(&self) -> Result<(), MailboxError> {
        self.is_shutdown.store(true, Ordering::Relaxed);
        // Release all retained volatile inboxes at transport shutdown.
        let mut inboxes = self.inboxes.write().await;
        for inbox in inboxes.values() {
            let mut state = inbox.state();
            state.subscription = None;
            state.retired = true;
            state.pending.clear();
            state.outstanding = 0;
            state.outstanding_bytes = 0;
            inbox.ready.notify_waiters();
        }
        inboxes.clear();
        self.total_direct_bytes.store(0, Ordering::Release);
        // Close all broadcast channels.
        self.broadcasts.write().await.clear();
        Ok(())
    }
}

// ─── InProcessStream ────────────────────────────────────────────────────────

/// Receives both direct and broadcast messages for a single agent.
struct InProcessStream {
    direct: Arc<DirectInbox>,
    subscription: MailboxSubscription,
    delivered: VecDeque<DirectEnvelope>,
    detached: bool,
    broadcast: Option<broadcast::Receiver<Arc<AgentMessage>>>,
    total_direct_bytes: Arc<AtomicUsize>,
}

impl InProcessStream {
    fn detach(&mut self) {
        if self.detached {
            return;
        }
        let mut state = self.direct.state();
        if !state.retired {
            for envelope in self.delivered.drain(..).rev() {
                state.pending.push_front(envelope);
            }
        } else {
            self.delivered.clear();
        }
        state.attached = false;
        self.detached = true;
        self.direct.ready.notify_waiters();
    }

    fn take_direct(&mut self) -> Option<Arc<AgentMessage>> {
        if self.detached {
            return None;
        }
        let envelope = {
            let mut state = self.direct.state();
            if state.retired || state.subscription.as_ref() != Some(&self.subscription) {
                return None;
            }
            state.pending.pop_front()
        }?;
        let message = Arc::clone(&envelope.message);
        self.delivered.push_back(envelope);
        Some(message)
    }
}

impl Drop for InProcessStream {
    fn drop(&mut self) {
        self.detach();
    }
}

#[async_trait]
impl MessageStream for InProcessStream {
    async fn recv(&mut self) -> Option<Arc<AgentMessage>> {
        loop {
            // Register the waiter before checking the queue to avoid a lost
            // notification between the empty check and the await.
            let inbox = Arc::clone(&self.direct);
            let notified = inbox.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(message) = self.try_recv() {
                return Some(message);
            }
            if self.detached
                || self.direct.state().subscription.as_ref() != Some(&self.subscription)
            {
                return None;
            }
            tokio::select! {
                biased;
                _ = &mut notified => {},
                result = async {
                    match self.broadcast.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => match result {
                    Ok(message) => return Some(message),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(target: "astra_messaging", n, "broadcast receiver lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => self.broadcast = None,
                },
            }
        }
    }

    fn try_recv(&mut self) -> Option<Arc<AgentMessage>> {
        if self.detached || self.direct.state().subscription.as_ref() != Some(&self.subscription) {
            return None;
        }
        // Try direct first.
        if let Some(msg) = self.take_direct() {
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

    async fn acknowledge(&mut self, message: &AgentMessage) -> Result<(), MailboxError> {
        if let Some(index) = self
            .delivered
            .iter()
            .position(|item| item.message.id == message.id)
        {
            let envelope = self.delivered.remove(index).expect("known delivery index");
            let mut state = self.direct.state();
            if !state.retired {
                state.outstanding -= 1;
                state.outstanding_bytes -= envelope.charged_bytes;
                self.total_direct_bytes
                    .fetch_sub(envelope.charged_bytes, Ordering::AcqRel);
            }
        }
        Ok(())
    }

    fn detach(&mut self) {
        InProcessStream::detach(self);
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
    async fn dropped_stream_preserves_accepted_order_and_capacity_for_reattach() {
        let transport = InProcessTransport::new();
        let receiver = addr("session", "root");
        let first = transport.register(receiver.clone(), None).await.unwrap();
        let mut stream = transport.subscribe(&first).await.unwrap();
        let sender = addr("child", "worker");
        let old = text_msg(sender.clone(), receiver.clone(), "old");
        let old_id = old.id.clone();
        transport.send(old).await.unwrap();
        assert_eq!(stream.try_recv().unwrap().id, old_id);
        let queued = text_msg(sender.clone(), receiver.clone(), "queued");
        let queued_id = queued.id.clone();
        transport.send(queued).await.unwrap();

        transport.unregister(&first).await.unwrap();
        drop(stream);
        let next = transport.register(receiver.clone(), None).await.unwrap();
        let mut next_stream = transport.subscribe(&next).await.unwrap();
        let fresh = text_msg(sender, receiver, "fresh");
        let fresh_id = fresh.id.clone();
        transport.send(fresh).await.unwrap();
        assert_eq!(next_stream.try_recv().unwrap().id, old_id);
        assert_eq!(next_stream.try_recv().unwrap().id, queued_id);
        assert_eq!(next_stream.try_recv().unwrap().id, fresh_id);
    }

    #[tokio::test]
    async fn unacknowledged_delivery_keeps_its_capacity_charge_across_reattach() {
        let transport = InProcessTransport::new();
        let receiver = addr("session", "root");
        let sender = addr("child", "worker");
        let first = transport.register(receiver.clone(), None).await.unwrap();
        let mut stream = transport.subscribe(&first).await.unwrap();
        for _ in 0..DIRECT_INBOX_CAPACITY {
            transport
                .send(text_msg(sender.clone(), receiver.clone(), "queued"))
                .await
                .unwrap();
        }
        let delivered = stream.try_recv().unwrap();
        assert!(
            transport
                .send(text_msg(sender.clone(), receiver.clone(), "over capacity"))
                .await
                .is_err()
        );
        transport.unregister(&first).await.unwrap();
        drop(stream);
        let next = transport.register(receiver.clone(), None).await.unwrap();
        let mut stream = transport.subscribe(&next).await.unwrap();
        assert_eq!(stream.try_recv().unwrap().id, delivered.id);
        assert!(
            transport
                .send(text_msg(sender.clone(), receiver.clone(), "still full"))
                .await
                .is_err()
        );
        stream.acknowledge(&delivered).await.unwrap();
        transport
            .send(text_msg(sender, receiver, "room after ack"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn byte_budget_rejects_oversized_and_releases_only_after_ack_or_retire() {
        let transport = InProcessTransport::new();
        let receiver = addr("session", "root");
        let sender = addr("child", "worker");
        let first = transport.register(receiver.clone(), None).await.unwrap();
        let mut stream = transport.subscribe(&first).await.unwrap();
        let oversized = text_msg(
            sender.clone(),
            receiver.clone(),
            &"x".repeat(MAX_DIRECT_MESSAGE_BYTES),
        );
        assert!(transport.send(oversized).await.is_err());
        assert_eq!(transport.total_direct_bytes.load(Ordering::Acquire), 0);

        let accepted = text_msg(sender.clone(), receiver.clone(), &"x".repeat(1024));
        let id = accepted.id.clone();
        transport.send(accepted).await.unwrap();
        let charge = transport.total_direct_bytes.load(Ordering::Acquire);
        assert!(charge > 1024);
        assert_eq!(stream.try_recv().unwrap().id, id);
        assert_eq!(transport.total_direct_bytes.load(Ordering::Acquire), charge);
        transport.unregister(&first).await.unwrap();
        drop(stream);
        let second = transport.register(receiver.clone(), None).await.unwrap();
        let mut stream = transport.subscribe(&second).await.unwrap();
        let replayed = stream.try_recv().unwrap();
        assert_eq!(replayed.id, id);
        stream.acknowledge(&replayed).await.unwrap();
        assert_eq!(transport.total_direct_bytes.load(Ordering::Acquire), 0);

        transport
            .send(text_msg(sender, receiver, "terminal pending"))
            .await
            .unwrap();
        let terminal = stream.try_recv().unwrap();
        transport.unregister(&second).await.unwrap();
        transport.retire(&second).await.unwrap();
        assert_eq!(transport.total_direct_bytes.load(Ordering::Acquire), 0);
        stream.acknowledge(&terminal).await.unwrap();
        assert_eq!(transport.total_direct_bytes.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn process_budget_backpressures_one_address_without_losing_another() {
        let transport = InProcessTransport::new();
        let first = addr("session-one", "root");
        let second = addr("session-two", "root");
        let sender = addr("child", "worker");
        let _first = transport.register(first.clone(), None).await.unwrap();
        let second_subscription = transport.register(second.clone(), None).await.unwrap();
        let mut second_stream = transport.subscribe(&second_subscription).await.unwrap();
        let existing = text_msg(sender.clone(), second.clone(), "already accepted");
        let id = existing.id.clone();
        transport.send(existing).await.unwrap();
        let reserved = transport.total_direct_bytes.load(Ordering::Acquire);
        transport
            .total_direct_bytes
            .store(MAX_TOTAL_DIRECT_BYTES - 1, Ordering::Release);
        assert!(
            transport
                .send(text_msg(sender, first, "would exceed global budget"))
                .await
                .is_err()
        );
        let received = second_stream.try_recv().unwrap();
        assert_eq!(received.id, id);
        transport
            .total_direct_bytes
            .store(reserved, Ordering::Release);
        second_stream.acknowledge(&received).await.unwrap();
        assert_eq!(transport.total_direct_bytes.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn inbox_byte_limit_backpressures_before_message_count_limit() {
        let transport = InProcessTransport::new();
        let receiver = addr("session", "root");
        let subscription = transport.register(receiver.clone(), None).await.unwrap();
        let mut stream = transport.subscribe(&subscription).await.unwrap();
        let sender = addr("child", "worker");
        let content = "x".repeat(120 * 1024);
        let mut accepted = 0;
        while transport
            .send(text_msg(sender.clone(), receiver.clone(), &content))
            .await
            .is_ok()
        {
            accepted += 1;
        }
        assert!(accepted > 1 && accepted < DIRECT_INBOX_CAPACITY);
        assert!(transport.total_direct_bytes.load(Ordering::Acquire) <= MAX_DIRECT_INBOX_BYTES);
        let received = stream.try_recv().unwrap();
        stream.acknowledge(&received).await.unwrap();
        transport
            .send(text_msg(sender, receiver, &content))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn thousand_session_inboxes_isolate_concurrent_direct_sends() {
        let transport = Arc::new(InProcessTransport::new());
        let sender = addr("source", "worker");
        let mut subscriptions = Vec::new();
        let mut expected = Vec::new();
        let mut sends = tokio::task::JoinSet::new();
        for index in 0..1_000 {
            let receiver = addr(&format!("session-{index}"), "root");
            subscriptions.push(transport.register(receiver.clone(), None).await.unwrap());
            let message = text_msg(sender.clone(), receiver, &format!("message-{index}"));
            expected.push(message.id.clone());
            let transport = Arc::clone(&transport);
            sends.spawn(async move { transport.send(message).await });
        }
        while let Some(sent) = sends.join_next().await {
            sent.unwrap().unwrap();
        }
        assert_eq!(transport.agent_count().await, 1_000);
        for (subscription, id) in subscriptions.iter().zip(expected) {
            let mut stream = transport.subscribe(subscription).await.unwrap();
            let received = stream.try_recv().unwrap();
            assert_eq!(received.id, id);
            stream.acknowledge(&received).await.unwrap();
            transport.unregister(subscription).await.unwrap();
            drop(stream);
            transport.retire(subscription).await.unwrap();
        }
        assert_eq!(transport.total_direct_bytes.load(Ordering::Acquire), 0);
        assert_eq!(transport.inboxes.read().await.len(), 0);
    }

    #[tokio::test]
    async fn retirement_reclaims_address_and_stale_owner_cannot_retire_successor() {
        let transport = InProcessTransport::new();
        let address = addr("session", "root");
        let old = transport.register(address.clone(), None).await.unwrap();
        transport.unregister(&old).await.unwrap();
        transport.retire(&old).await.unwrap();
        assert_eq!(transport.inboxes.read().await.len(), 0);
        let next = transport.register(address.clone(), None).await.unwrap();
        transport.retire(&old).await.unwrap();
        let mut stream = transport.subscribe(&next).await.unwrap();
        let message = text_msg(addr("child", "worker"), address, "new owner");
        let id = message.id.clone();
        transport.send(message).await.unwrap();
        assert_eq!(stream.try_recv().unwrap().id, id);
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
    async fn unregister_detaches_but_terminal_retirement_closes_the_address() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");

        let _a_subscription = transport.register(a.clone(), None).await.unwrap();
        assert_eq!(transport.agent_count().await, 1);

        transport.unregister(&_a_subscription).await.unwrap();
        assert_eq!(transport.agent_count().await, 0);

        let msg = text_msg(addr("r0", "x"), a.clone(), "hello");
        transport.send(msg.clone()).await.unwrap();
        let resumed = transport.register(a.clone(), None).await.unwrap();
        let mut stream = transport.subscribe(&resumed).await.unwrap();
        assert_eq!(stream.try_recv().unwrap().id, msg.id);
        drop(stream);
        transport.unregister(&resumed).await.unwrap();
        transport.retire(&resumed).await.unwrap();
        assert!(transport.send(msg).await.is_err());
    }

    #[tokio::test]
    async fn active_owner_cannot_be_replaced_and_stale_cleanup_cannot_close_successor() {
        let transport = InProcessTransport::new();
        let address = addr("reused-run", "agent");
        let old = transport.register(address.clone(), None).await.unwrap();
        let old_stream = transport.subscribe(&old).await.unwrap();
        assert!(
            transport.subscribe(&old).await.is_err(),
            "registration attaches only once"
        );
        assert!(transport.register(address.clone(), None).await.is_err());
        transport.unregister(&old).await.unwrap();
        assert!(transport.register(address.clone(), None).await.is_err());
        drop(old_stream);
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
    async fn detached_stream_can_reattach_before_async_stale_cleanup() {
        let transport = InProcessTransport::new();
        let address = addr("session", "root");
        let old = transport.register(address.clone(), None).await.unwrap();
        let stream = transport.subscribe(&old).await.unwrap();
        let message = text_msg(addr("child", "worker"), address.clone(), "retained");
        let id = message.id.clone();
        transport.send(message).await.unwrap();
        drop(stream);
        let next = transport.register(address, None).await.unwrap();
        transport.unregister(&old).await.unwrap();
        let mut stream = transport.subscribe(&next).await.unwrap();
        assert_eq!(stream.try_recv().unwrap().id, id);
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
    async fn full_inbox_rejects_additional_messages() {
        let transport = InProcessTransport::new();
        let a = addr("r1", "a");
        let b = addr("r2", "b");

        let _a_subscription = transport.register(a.clone(), None).await.unwrap();
        let _b_subscription = transport.register(b.clone(), None).await.unwrap();
        // Subscribe retains deliveries within the bounded inbox.
        let _stream = transport.subscribe(&_b_subscription).await.unwrap();

        // Fill the inbox beyond capacity.
        let mut sent = 0u64;
        let mut dropped = 0u64;
        for i in 0..5000 {
            let msg = text_msg(a.clone(), b.clone(), &format!("flood-{i}"));
            match transport.send(msg).await {
                Ok(()) => sent += 1,
                Err(MailboxError::DeliveryRejected(_)) => dropped += 1,
                Err(error) => panic!("unexpected send failure: {error}"),
            }
        }

        assert_eq!(sent, DIRECT_INBOX_CAPACITY as u64);
        assert_eq!(dropped, 5000 - sent);
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
