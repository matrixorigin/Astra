//! Transport trait — abstracts the message delivery mechanism.
//!
//! Two implementations are planned:
//! - [`InProcessTransport`](super::in_process::InProcessTransport): tokio channels (CLI, µs latency)
//! - Future `DatabaseTransport`: existing RunEngine event log (Cloud, ~10ms latency)

use std::sync::Arc;

use async_trait::async_trait;

use super::types::{AgentAddress, AgentMessage, MailboxError};

/// Authority to close one registration, never whichever registration happens
/// to occupy the same logical address later. Keep the returned value with the
/// mailbox/cleanup owner; do not resolve it again when cleaning up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxSubscription {
    address: AgentAddress,
    id: String,
}

impl MailboxSubscription {
    pub fn new(address: AgentAddress) -> Self {
        Self {
            address,
            id: uuid::Uuid::new_v4().simple().to_string(),
        }
    }

    pub fn address(&self) -> &AgentAddress {
        &self.address
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl std::ops::Deref for MailboxSubscription {
    type Target = AgentAddress;
    fn deref(&self) -> &Self::Target {
        &self.address
    }
}

impl std::fmt::Display for MailboxSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.address.fmt(f)
    }
}

// ─── MessageStream ──────────────────────────────────────────────────────────

/// An async stream of messages for a single agent.
///
/// Returned by [`MessageTransport::subscribe`]. Implementations must be
/// cancel-safe (dropping the stream doesn't lose messages).
#[async_trait]
pub trait MessageStream: Send {
    /// Wait for the next message. Returns `None` when the stream is closed.
    async fn recv(&mut self) -> Option<Arc<AgentMessage>>;

    /// Non-blocking: return a message if one is already buffered.
    fn try_recv(&mut self) -> Option<Arc<AgentMessage>>;

    /// Confirm that a delivered message has been consumed by the runtime.
    /// Durable transports override this; in-process delivery is already owned
    /// by the receiver once dequeued and therefore uses the no-op default.
    async fn acknowledge(&mut self, _message: &AgentMessage) -> Result<(), MailboxError> {
        Ok(())
    }

    /// Original durable claim authority for this exact delivered envelope.
    /// Never resolve the current database token by message ID to acknowledge
    /// an older delivery. In-memory transports have no durable claim.
    fn claim_token(&self, _message: &AgentMessage) -> Option<&str> {
        None
    }

    /// Drain all currently buffered messages without blocking.
    fn drain(&mut self) -> Vec<Arc<AgentMessage>> {
        let mut msgs = Vec::new();
        while let Some(m) = self.try_recv() {
            msgs.push(m);
        }
        msgs
    }

    /// Drain up to `limit` buffered messages. Returns `true` if more remain.
    fn drain_bounded(&mut self, limit: usize) -> (Vec<Arc<AgentMessage>>, bool) {
        let mut msgs = Vec::with_capacity(limit);
        while msgs.len() < limit {
            match self.try_recv() {
                Some(m) => msgs.push(m),
                None => return (msgs, false),
            }
        }
        // Check if there's at least one more (peek without consuming isn't
        // available, but this single extra message is acceptable).
        match self.try_recv() {
            Some(m) => {
                msgs.push(m);
                (msgs, true) // limit+1 messages, more may exist
            }
            None => (msgs, false),
        }
    }
}

// ─── MessageTransport ───────────────────────────────────────────────────────

/// Pluggable message transport for inter-agent communication.
///
/// Follows the same trait-per-deployment pattern as `SubRunExecutor`
/// (CLI impl vs Server impl).
#[async_trait]
pub trait MessageTransport: Send + Sync {
    /// Whether unregistering a subscriber preserves its unacknowledged
    /// deliveries for a later registration at the same logical address.
    /// Volatile transports must hand off unread envelopes before closing.
    fn recovers_unacknowledged_on_unregister(&self) -> bool {
        false
    }

    /// Register an agent so it can receive messages.
    ///
    /// `delegation_id` — if provided, the agent joins a broadcast group.
    async fn register(
        &self,
        addr: AgentAddress,
        delegation_id: Option<String>,
    ) -> Result<MailboxSubscription, MailboxError>;

    /// Close only this subscription. A stale handle is an idempotent no-op.
    async fn unregister(&self, subscription: &MailboxSubscription) -> Result<(), MailboxError>;

    /// Subscribe to messages for this agent.
    ///
    /// Attach once to the registration returned by `register`. Replacing a
    /// subscriber requires a fresh registration (and hence fresh authority).
    async fn subscribe(
        &self,
        subscription: &MailboxSubscription,
    ) -> Result<Box<dyn MessageStream>, MailboxError>;

    /// Resolve an agent inside one delegation namespace. Implementations for
    /// distributed deployments must use shared state, not process-local
    /// registration caches.
    async fn resolve_agent(
        &self,
        delegation_id: &str,
        agent_id: &str,
    ) -> Result<AgentAddress, MailboxError>;

    /// List live members of one delegation namespace.
    async fn list_agents(&self, delegation_id: &str) -> Result<Vec<AgentAddress>, MailboxError>;

    /// Send a message to a single agent (`MessageTarget::Direct`).
    async fn send(&self, msg: Arc<AgentMessage>) -> Result<(), MailboxError>;

    /// Broadcast a message to all agents in a delegation group.
    async fn broadcast(
        &self,
        delegation_id: &str,
        msg: Arc<AgentMessage>,
    ) -> Result<(), MailboxError>;

    /// Health check. Returns Ok if the transport is operational.
    /// Default implementation assumes healthy.
    async fn health_check(&self) -> Result<(), MailboxError> {
        Ok(())
    }

    /// Graceful shutdown. Implementations should drain in-flight messages.
    /// Default implementation is a no-op.
    async fn shutdown(&self) -> Result<(), MailboxError> {
        Ok(())
    }
}
