//! Transport trait — abstracts the message delivery mechanism.
//!
//! Two implementations are planned:
//! - [`InProcessTransport`](super::in_process::InProcessTransport): tokio channels (CLI, µs latency)
//! - Future `DatabaseTransport`: existing RunEngine event log (Cloud, ~10ms latency)

use std::sync::Arc;

use async_trait::async_trait;

use super::types::{AgentAddress, AgentMessage, MailboxError};

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

    /// Drain all currently buffered messages without blocking.
    fn drain(&mut self) -> Vec<Arc<AgentMessage>> {
        let mut msgs = Vec::new();
        while let Some(m) = self.try_recv() {
            msgs.push(m);
        }
        msgs
    }
}

// ─── MessageTransport ───────────────────────────────────────────────────────

/// Pluggable message transport for inter-agent communication.
///
/// Follows the same trait-per-deployment pattern as `SubRunExecutor`
/// (CLI impl vs Server impl).
#[async_trait]
pub trait MessageTransport: Send + Sync {
    /// Register an agent so it can receive messages.
    ///
    /// `delegation_id` — if provided, the agent joins a broadcast group.
    async fn register(
        &self,
        addr: AgentAddress,
        delegation_id: Option<String>,
    ) -> Result<(), MailboxError>;

    /// Unregister an agent. Its message stream will be closed.
    async fn unregister(&self, addr: &AgentAddress) -> Result<(), MailboxError>;

    /// Subscribe to messages for this agent.
    ///
    /// Must be called after `register`. Returns a stream that yields messages
    /// addressed to this agent (both direct and broadcast).
    async fn subscribe(
        &self,
        addr: &AgentAddress,
    ) -> Result<Box<dyn MessageStream>, MailboxError>;

    /// Send a message to a single agent (`MessageTarget::Direct`).
    async fn send(&self, msg: Arc<AgentMessage>) -> Result<(), MailboxError>;

    /// Broadcast a message to all agents in a delegation group.
    async fn broadcast(
        &self,
        delegation_id: &str,
        msg: Arc<AgentMessage>,
    ) -> Result<(), MailboxError>;
}
