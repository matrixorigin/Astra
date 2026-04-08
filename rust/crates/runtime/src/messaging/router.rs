//! Agent mailbox router — transport-agnostic message dispatching.
//!
//! Resolves high-level targets (`Parent`, `Broadcast`) into concrete delivery
//! actions using the delegation tracker and the pluggable transport.

use std::sync::Arc;

use super::transport::{MessageStream, MessageTransport};
use super::types::{AgentAddress, AgentMessage, MailboxError, MessageTarget};
use crate::server::delegation_engine::DelegationTracker;

// ─── AgentMailbox ───────────────────────────────────────────────────────────

/// An agent's handle for sending and receiving messages.
///
/// Created by [`AgentMailboxRouter::register`] and passed into the agentic loop
/// via `AgenticLoopState` or `SubRunConfig`.
///
/// The stream is wrapped in a `Mutex` so that the mailbox is `Send + Sync`,
/// which is required by the tokio-spawned agentic loop futures.
pub struct AgentMailbox {
    /// This agent's address.
    pub address: AgentAddress,
    /// Delegation group this agent belongs to (if any).
    pub delegation_id: Option<String>,
    /// Message receive stream (direct + broadcast), mutex-guarded for Sync.
    stream: tokio::sync::Mutex<Box<dyn MessageStream>>,
    /// Router reference for sending.
    router: Arc<AgentMailboxRouter>,
}

impl AgentMailbox {
    /// Non-blocking: get the next available message, if any.
    pub fn try_recv(&mut self) -> Option<Arc<AgentMessage>> {
        self.stream.get_mut().try_recv()
    }

    /// Blocking: wait for the next message.
    pub async fn recv(&self) -> Option<Arc<AgentMessage>> {
        self.stream.lock().await.recv().await
    }

    /// Drain all currently buffered messages.
    pub fn drain(&mut self) -> Vec<Arc<AgentMessage>> {
        self.stream.get_mut().drain()
    }

    /// Send a message through the router (handles target resolution).
    pub async fn send(&self, msg: AgentMessage) -> Result<(), MailboxError> {
        self.router.send(msg).await
    }

    /// Convenience: send a text message to the parent agent.
    pub async fn send_to_parent(
        &self,
        content: impl Into<String>,
    ) -> Result<(), MailboxError> {
        let msg = AgentMessage::new(
            self.address.clone(),
            MessageTarget::Parent,
            super::types::MessagePayload::Text {
                content: content.into(),
                summary: None,
            },
        );
        self.router.send(msg).await
    }

    /// Convenience: send a progress update to the parent agent.
    pub async fn send_progress(
        &self,
        turn_index: u32,
        tool_calls: u32,
        status: &str,
        detail: Option<String>,
    ) -> Result<(), MailboxError> {
        let msg = AgentMessage::new(
            self.address.clone(),
            MessageTarget::Parent,
            super::types::MessagePayload::Progress {
                turn_index,
                tool_calls,
                status: status.into(),
                detail,
            },
        );
        self.router.send(msg).await
    }
}

// ─── AgentMailboxRouter ─────────────────────────────────────────────────────

/// Central message router that resolves targets and dispatches via a transport.
///
/// Transport-agnostic: works with `InProcessTransport` (CLI, µs latency)
/// or a future `DatabaseTransport` (Cloud, ~10ms latency) interchangeably.
pub struct AgentMailboxRouter {
    transport: Arc<dyn MessageTransport>,
    delegation_tracker: Arc<DelegationTracker>,
    /// run_id → registered AgentAddress (for resolving Parent targets).
    address_registry: tokio::sync::RwLock<std::collections::HashMap<String, AgentAddress>>,
    /// agent_id → AgentAddress (for resolving agent_id-only Direct targets from send_tool).
    agent_id_index: tokio::sync::RwLock<std::collections::HashMap<String, AgentAddress>>,
}

impl AgentMailboxRouter {
    pub fn new(
        transport: Arc<dyn MessageTransport>,
        delegation_tracker: Arc<DelegationTracker>,
    ) -> Self {
        Self {
            transport,
            delegation_tracker,
            address_registry: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            agent_id_index: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Register an agent and return its mailbox handle.
    pub async fn register(
        self: &Arc<Self>,
        addr: AgentAddress,
        delegation_id: Option<String>,
    ) -> Result<AgentMailbox, MailboxError> {
        self.transport
            .register(addr.clone(), delegation_id.clone())
            .await?;

        {
            let mut reg = self.address_registry.write().await;
            reg.insert(addr.run_id.clone(), addr.clone());
        }
        {
            let mut idx = self.agent_id_index.write().await;
            idx.insert(addr.agent_id.clone(), addr.clone());
        }

        let stream = self.transport.subscribe(&addr).await?;

        Ok(AgentMailbox {
            address: addr,
            delegation_id,
            stream: tokio::sync::Mutex::new(stream),
            router: Arc::clone(self),
        })
    }

    /// Unregister an agent (typically on completion or failure).
    pub async fn unregister(&self, addr: &AgentAddress) -> Result<(), MailboxError> {
        self.address_registry.write().await.remove(&addr.run_id);
        self.agent_id_index.write().await.remove(&addr.agent_id);
        self.transport.unregister(addr).await
    }

    /// Resolve the address of a parent run.
    async fn resolve_parent_addr(&self, child_run_id: &str) -> Result<AgentAddress, MailboxError> {
        let parent_run_id = self
            .delegation_tracker
            .get_parent(child_run_id)
            .await
            .ok_or(MailboxError::NoParent)?;

        // Try address registry first (includes root agents).
        if let Some(addr) = self.address_registry.read().await.get(&parent_run_id) {
            return Ok(addr.clone());
        }

        // Fall back to delegation tracker (for agents registered before router).
        let agent_id = self
            .delegation_tracker
            .get_agent_id(&parent_run_id)
            .await
            .unwrap_or_default();

        Ok(AgentAddress::new(&parent_run_id, &agent_id))
    }

    /// Resolve a Direct target that may have an empty run_id (from send_tool).
    ///
    /// The send_tool only knows the peer's agent_id, not its run_id. This method
    /// looks up the full address from the agent_id_index populated during register().
    async fn resolve_direct_addr(&self, address: &AgentAddress) -> AgentAddress {
        if !address.run_id.is_empty() {
            return address.clone();
        }
        // Empty run_id — resolve by agent_id.
        if let Some(full_addr) = self.agent_id_index.read().await.get(&address.agent_id) {
            return full_addr.clone();
        }
        // Not found in index — return as-is, transport will return AgentNotFound.
        address.clone()
    }

    /// Send a message, resolving `Parent`, `Broadcast`, and agent_id-only `Direct` targets.
    pub async fn send(&self, msg: AgentMessage) -> Result<(), MailboxError> {
        let target = msg.to.clone();
        match target {
            MessageTarget::Direct { ref address } => {
                let resolved = self.resolve_direct_addr(address).await;
                let resolved_msg = AgentMessage {
                    to: MessageTarget::Direct { address: resolved },
                    ..msg
                };
                self.transport.send(Arc::new(resolved_msg)).await
            }
            MessageTarget::Broadcast { delegation_id } => {
                self.transport
                    .broadcast(&delegation_id, Arc::new(msg))
                    .await
            }
            MessageTarget::Parent => {
                let parent_addr = self.resolve_parent_addr(&msg.from.run_id).await?;
                let resolved_msg = AgentMessage {
                    to: MessageTarget::Direct {
                        address: parent_addr,
                    },
                    ..msg
                };
                self.transport.send(Arc::new(resolved_msg)).await
            }
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::in_process::InProcessTransport;
    use crate::messaging::types::{MessagePayload, MessageTarget};

    fn tracker() -> Arc<DelegationTracker> {
        Arc::new(DelegationTracker::new())
    }

    fn addr(run: &str, agent: &str) -> AgentAddress {
        AgentAddress::new(run, agent)
    }

    #[tokio::test]
    async fn register_and_send_direct() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker()));

        let a = addr("r1", "coder");
        let b = addr("r2", "reviewer");

        let _mailbox_a = router.register(a.clone(), None).await.unwrap();
        let mut mailbox_b = router.register(b.clone(), None).await.unwrap();

        let msg = AgentMessage::new(
            a.clone(),
            MessageTarget::Direct { address: b.clone() },
            MessagePayload::Text {
                content: "check this".into(),
                summary: None,
            },
        );
        router.send(msg).await.unwrap();

        let received = mailbox_b.try_recv().unwrap();
        match &received.payload {
            MessagePayload::Text { content, .. } => assert_eq!(content, "check this"),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn mailbox_send_convenience() {
        let transport = Arc::new(InProcessTransport::new());
        let dt = tracker();
        let router = Arc::new(AgentMailboxRouter::new(transport, dt.clone()));

        let parent = addr("r0", "orchestrator");
        let child = addr("r1", "worker");

        let mut parent_mailbox = router.register(parent.clone(), None).await.unwrap();
        let child_mailbox = router.register(child.clone(), None).await.unwrap();

        // Set up parent relationship via SubRunRecord.
        use crate::server::delegation_engine::SubRunRecord;
        dt.record_sub_run(SubRunRecord {
            run_id: "r1".into(),
            parent_run_id: "r0".into(),
            delegation_id: "del-test".into(),
            agent_id: "worker".into(),
            depth: 1,
        })
        .await;

        child_mailbox.send_to_parent("done!").await.unwrap();

        let received = parent_mailbox.try_recv().unwrap();
        match &received.payload {
            MessagePayload::Text { content, .. } => assert_eq!(content, "done!"),
            _ => panic!("expected text"),
        }
    }
}
