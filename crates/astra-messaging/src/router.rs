//! Agent mailbox router — transport-agnostic message dispatching.
//!
//! Resolves high-level targets (`Parent`, `Broadcast`) into concrete delivery
//! actions using the delegation tracker and the pluggable transport.

use std::collections::VecDeque;
use std::sync::Arc;

use super::delegation::{DelegationLookup, SubRunInfo};
use super::transport::{MessageStream, MessageTransport};
use super::types::{AgentAddress, AgentMessage, MailboxError, MessageTarget};

/// Result of a permission request sent via [`AgentMailbox::request_permission`].
#[derive(Debug, Clone)]
pub struct PermissionOutcome {
    /// Whether the parent accepted the request.
    pub accepted: bool,
    /// Optional response data (caller deserializes as appropriate).
    pub data: Option<serde_json::Value>,
}

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
    /// Messages buffered while waiting for a correlated response.
    buffered: tokio::sync::Mutex<VecDeque<Arc<AgentMessage>>>,
    /// Router reference for sending.
    router: Arc<AgentMailboxRouter>,
}

impl AgentMailbox {
    /// True if this agent has a parent in the delegation hierarchy.
    pub async fn has_parent(&self) -> bool {
        self.router
            .delegation_tracker
            .get_parent(&self.address.run_id)
            .await
            .is_some()
    }

    /// Clone the shared router so tools can send additional messages in-turn.
    pub fn router(&self) -> Arc<AgentMailboxRouter> {
        self.router.clone()
    }

    /// Non-blocking: get the next available message, if any.
    pub fn try_recv(&mut self) -> Option<Arc<AgentMessage>> {
        if let Some(msg) = self.buffered.get_mut().pop_front() {
            return Some(msg);
        }
        self.stream.get_mut().try_recv()
    }

    /// Blocking: wait for the next message.
    pub async fn recv(&self) -> Option<Arc<AgentMessage>> {
        if let Some(msg) = self.buffered.lock().await.pop_front() {
            return Some(msg);
        }
        self.stream.lock().await.recv().await
    }

    /// Drain all currently buffered messages.
    pub fn drain(&mut self) -> Vec<Arc<AgentMessage>> {
        let mut buffered: Vec<_> = self.buffered.get_mut().drain(..).collect();
        buffered.extend(self.stream.get_mut().drain());
        buffered
    }

    /// Drain up to `limit` messages. Returns `true` if more remain.
    pub fn drain_bounded(&mut self, limit: usize) -> (Vec<Arc<AgentMessage>>, bool) {
        let mut msgs = Vec::with_capacity(limit);
        while msgs.len() < limit {
            match self.try_recv() {
                Some(msg) => msgs.push(msg),
                None => return (msgs, false),
            }
        }

        match self.try_recv() {
            Some(extra) => {
                self.buffered.get_mut().push_back(extra);
                (msgs, true)
            }
            None => (msgs, false),
        }
    }

    /// Confirm durable consumption after the caller has converted the messages
    /// into runtime state. A failed confirmation leaves the transport claim
    /// recoverable for redelivery instead of silently losing the message.
    pub async fn acknowledge_received(
        &self,
        messages: &[Arc<AgentMessage>],
    ) -> Result<(), MailboxError> {
        let mut stream = self.stream.lock().await;
        let mut first_error = None;
        for message in messages {
            if let Err(error) = stream.acknowledge(message).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Send a message through the router (handles target resolution).
    pub async fn send(&self, msg: AgentMessage) -> Result<(), MailboxError> {
        self.router.send(msg).await
    }

    /// Convenience: send a text message to the parent agent.
    pub async fn send_to_parent(&self, content: impl Into<String>) -> Result<(), MailboxError> {
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

    /// Send a permission request to the parent agent and wait for response.
    ///
    /// This is used by child agents running in background mode to request
    /// approval for tools that would normally require user interaction.
    ///
    /// Takes a serializable request and returns the raw response data along
    /// with the accepted flag. The caller is responsible for deserializing
    /// the response into the appropriate type (e.g., `PermissionResponse`).
    pub async fn request_permission(
        &mut self,
        request: impl serde::Serialize,
        timeout: std::time::Duration,
    ) -> Result<PermissionOutcome, MailboxError> {
        use crate::types::{MessagePayload, RequestType};

        let mut skipped = VecDeque::new();

        // Build and send the request message
        let request_id = uuid::Uuid::new_v4().to_string();
        let data = serde_json::to_value(&request)
            .map_err(|e| MailboxError::Transport(format!("serialize permission request: {e}")))?;
        let msg = AgentMessage::new(
            self.address.clone(),
            MessageTarget::Parent,
            MessagePayload::Request {
                request_type: RequestType::ToolPermission,
                data,
            },
        )
        .with_correlation(&request_id);

        self.router.send(msg).await?;

        // Wait for response with timeout
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                self.buffered.get_mut().extend(skipped.drain(..));
                return Err(MailboxError::Timeout(format!(
                    "Permission request timed out after {:?}",
                    timeout
                )));
            }

            let next_message = if let Some(msg) = self.buffered.get_mut().pop_front() {
                Ok(Some(msg))
            } else {
                tokio::time::timeout(remaining, self.stream.lock().await.recv()).await
            };

            match next_message {
                Ok(Some(msg)) => {
                    // Check if this is our response
                    if msg.correlation_id.as_deref() == Some(&request_id) {
                        self.buffered.get_mut().extend(skipped.drain(..));
                        self.stream.lock().await.acknowledge(&msg).await?;
                        if let MessagePayload::Response { data, accepted, .. } = &msg.payload {
                            return Ok(PermissionOutcome {
                                accepted: *accepted,
                                data: data.clone(),
                            });
                        }
                        return Err(MailboxError::Protocol(format!(
                            "expected response payload for permission request {request_id}, got {:?}",
                            msg.payload
                        )));
                    }
                    skipped.push_back(msg);
                }
                Ok(None) => {
                    self.buffered.get_mut().extend(skipped.drain(..));
                    return Err(MailboxError::Disconnected);
                }
                Err(_) => {
                    self.buffered.get_mut().extend(skipped.drain(..));
                    return Err(MailboxError::Timeout(format!(
                        "Permission request timed out after {:?}",
                        timeout
                    )));
                }
            }
        }
    }
}

/// Safety-net cleanup: unregister the mailbox from the router on drop.
///
/// Explicit `router.unregister()` calls at usage sites remain the primary
/// cleanup mechanism. This Drop impl catches cases where a mailbox is
/// dropped without explicit cleanup (e.g., child agents in delegation).
///
/// Uses `tokio::task::spawn` because `unregister` is async and `Drop` is sync.
/// The spawned task is fire-and-forget — if the runtime is shutting down,
/// the unregister may not complete, but that's acceptable since the transport
/// is being torn down anyway.
impl Drop for AgentMailbox {
    fn drop(&mut self) {
        let router = Arc::clone(&self.router);
        let addr = self.address.clone();
        // Best-effort: spawn only if a tokio runtime is available.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = router.unregister(&addr).await {
                    tracing::debug!(
                        target: "astra_runtime::messaging",
                        addr = %addr,
                        error = ?e,
                        "mailbox drop: unregister failed (may already be cleaned up)",
                    );
                }
            });
        }
    }
}

// ─── AgentMailboxRouter ─────────────────────────────────────────────────────

/// Central message router that resolves targets and dispatches via a transport.
///
/// Transport-agnostic: works with `InProcessTransport` (CLI, µs latency)
/// or a future `DatabaseTransport` (Cloud, ~10ms latency) interchangeably.
pub struct AgentMailboxRouter {
    transport: Arc<dyn MessageTransport>,
    delegation_tracker: Arc<dyn DelegationLookup>,
    /// run_id → registered AgentAddress (for resolving Parent targets).
    address_registry: tokio::sync::RwLock<std::collections::HashMap<String, AgentAddress>>,
    /// Serializes check-and-register so registration ownership is atomic.
    registration_gate: tokio::sync::Mutex<()>,
}

impl AgentMailboxRouter {
    pub fn new(
        transport: Arc<dyn MessageTransport>,
        delegation_tracker: Arc<dyn DelegationLookup>,
    ) -> Self {
        Self {
            transport,
            delegation_tracker,
            address_registry: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            registration_gate: tokio::sync::Mutex::new(()),
        }
    }

    /// Register an agent and return its mailbox handle.
    pub async fn register(
        self: &Arc<Self>,
        addr: AgentAddress,
        delegation_id: Option<String>,
    ) -> Result<AgentMailbox, MailboxError> {
        let _registration = self.registration_gate.lock().await;
        self.register_inner(addr, delegation_id).await
    }

    async fn register_inner(
        self: &Arc<Self>,
        addr: AgentAddress,
        delegation_id: Option<String>,
    ) -> Result<AgentMailbox, MailboxError> {
        self.transport
            .register(addr.clone(), delegation_id.clone())
            .await?;

        self.address_registry
            .write()
            .await
            .insert(addr.run_id.clone(), addr.clone());

        let stream = match self.transport.subscribe(&addr).await {
            Ok(stream) => stream,
            Err(err) => {
                let mut reg = self.address_registry.write().await;
                if reg.get(&addr.run_id) == Some(&addr) {
                    reg.remove(&addr.run_id);
                }
                drop(reg);
                if let Err(unregister_err) = self.transport.unregister(&addr).await {
                    tracing::warn!(
                        target: "astra_runtime::messaging",
                        addr = %addr,
                        error = ?unregister_err,
                        "failed to roll back transport registration after subscribe error",
                    );
                }
                return Err(err);
            }
        };

        Ok(AgentMailbox {
            address: addr,
            delegation_id,
            stream: tokio::sync::Mutex::new(stream),
            buffered: tokio::sync::Mutex::new(VecDeque::new()),
            router: Arc::clone(self),
        })
    }

    /// Unregister an agent (typically on completion or failure).
    pub async fn unregister(&self, addr: &AgentAddress) -> Result<(), MailboxError> {
        let _registration = self.registration_gate.lock().await;
        self.address_registry.write().await.remove(&addr.run_id);
        self.transport.unregister(addr).await
    }

    /// Record a sub-run relationship for parent-target resolution.
    pub async fn record_sub_run(&self, info: SubRunInfo) {
        self.delegation_tracker.record_sub_run(info).await;
    }

    /// Get the known delegation depth for a run.
    pub async fn run_depth(&self, run_id: &str) -> Option<u32> {
        self.delegation_tracker.get_depth(run_id).await
    }

    /// List live agents in one delegation namespace.
    pub async fn list_registered_agents(
        &self,
        delegation_id: &str,
    ) -> Result<Vec<AgentAddress>, MailboxError> {
        self.transport.list_agents(delegation_id).await
    }

    pub async fn resolve_agent(
        &self,
        delegation_id: &str,
        agent_id: &str,
    ) -> Result<AgentAddress, MailboxError> {
        self.transport.resolve_agent(delegation_id, agent_id).await
    }

    /// Check whether a specific run_id is registered in the address registry.
    pub async fn is_run_registered(&self, run_id: &str) -> bool {
        self.address_registry.read().await.contains_key(run_id)
    }

    /// Register an agent only if its run_id is not already registered.
    ///
    /// Returns `Ok(Some(mailbox))` if newly registered, `Ok(None)` if already
    /// present (no-op), or `Err` on transport failure.
    ///
    /// This prevents clobbering a caller's pre-registered mailbox.
    pub async fn register_if_absent(
        self: &Arc<Self>,
        addr: AgentAddress,
        delegation_id: Option<String>,
    ) -> Result<Option<AgentMailbox>, MailboxError> {
        let _registration = self.registration_gate.lock().await;
        if self
            .address_registry
            .read()
            .await
            .contains_key(&addr.run_id)
        {
            return Ok(None);
        }
        self.register_inner(addr, delegation_id).await.map(Some)
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
            .filter(|id| !id.is_empty());

        match agent_id {
            Some(id) => Ok(AgentAddress::new(&parent_run_id, &id)),
            None => {
                // Parent is the orchestrator/root run — not itself a sub-run,
                // so it has no agent_id in the tracker. Use a synthetic address
                // so progress messages can still be routed (best-effort).
                tracing::debug!(
                    target: "astra_runtime::messaging",
                    parent_run_id = %parent_run_id,
                    child_run_id = %child_run_id,
                    "parent run_id not found in address registry or tracker; \
                     using synthetic 'orchestrator' agent_id",
                );
                Ok(AgentAddress::new(&parent_run_id, "orchestrator"))
            }
        }
    }

    /// Send a message, resolving `Parent` and `Broadcast` targets. Direct
    /// targets must already carry their canonical run identity.
    pub async fn send(&self, msg: AgentMessage) -> Result<(), MailboxError> {
        let target = msg.to.clone();
        match target {
            MessageTarget::Direct { ref address } => {
                if address.run_id.is_empty() {
                    return Err(MailboxError::InvalidAddress(address.clone()));
                }
                self.transport.send(Arc::new(msg)).await
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
    use crate::in_process::InProcessTransport;
    use crate::types::{MessagePayload, MessageTarget};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tokio::sync::RwLock;

    struct AckRecordingStream {
        acknowledged: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl MessageStream for AckRecordingStream {
        async fn recv(&mut self) -> Option<Arc<AgentMessage>> {
            None
        }

        fn try_recv(&mut self) -> Option<Arc<AgentMessage>> {
            None
        }

        async fn acknowledge(&mut self, message: &AgentMessage) -> Result<(), MailboxError> {
            self.acknowledged
                .lock()
                .expect("ack recorder lock")
                .push(message.id.clone());
            Ok(())
        }
    }

    /// Simple in-memory mock for DelegationLookup (no runtime dependency).
    struct MockDelegation {
        parents: RwLock<HashMap<String, String>>,
        agents: RwLock<HashMap<String, String>>,
        depths: RwLock<HashMap<String, u32>>,
    }

    impl MockDelegation {
        fn new() -> Self {
            Self {
                parents: RwLock::new(HashMap::new()),
                agents: RwLock::new(HashMap::new()),
                depths: RwLock::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl DelegationLookup for MockDelegation {
        async fn get_parent(&self, run_id: &str) -> Option<String> {
            self.parents.read().await.get(run_id).cloned()
        }
        async fn get_agent_id(&self, run_id: &str) -> Option<String> {
            self.agents.read().await.get(run_id).cloned()
        }
        async fn get_depth(&self, run_id: &str) -> Option<u32> {
            self.depths.read().await.get(run_id).copied()
        }
        async fn record_sub_run(&self, info: SubRunInfo) {
            self.parents
                .write()
                .await
                .insert(info.run_id.clone(), info.parent_run_id.clone());
            self.agents
                .write()
                .await
                .insert(info.run_id.clone(), info.agent_id.clone());
            self.depths
                .write()
                .await
                .insert(info.run_id.clone(), info.depth);
        }
    }

    fn tracker() -> Arc<dyn DelegationLookup> {
        Arc::new(MockDelegation::new())
    }

    fn addr(run: &str, agent: &str) -> AgentAddress {
        AgentAddress::new(run, agent)
    }

    #[derive(Default)]
    struct FailingSubscribeTransport {
        registered: AtomicUsize,
        unregistered: AtomicUsize,
    }

    #[async_trait]
    impl MessageTransport for FailingSubscribeTransport {
        async fn register(
            &self,
            _addr: AgentAddress,
            _delegation_id: Option<String>,
        ) -> Result<(), MailboxError> {
            self.registered.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(())
        }
        async fn unregister(&self, _addr: &AgentAddress) -> Result<(), MailboxError> {
            self.unregistered.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(())
        }
        async fn subscribe(
            &self,
            _addr: &AgentAddress,
        ) -> Result<Box<dyn MessageStream>, MailboxError> {
            Err(MailboxError::Transport("subscribe failed".into()))
        }
        async fn resolve_agent(
            &self,
            _delegation_id: &str,
            agent_id: &str,
        ) -> Result<AgentAddress, MailboxError> {
            Err(MailboxError::AgentNotFound(AgentAddress::new("", agent_id)))
        }
        async fn list_agents(
            &self,
            _delegation_id: &str,
        ) -> Result<Vec<AgentAddress>, MailboxError> {
            Ok(Vec::new())
        }
        async fn send(&self, _msg: Arc<AgentMessage>) -> Result<(), MailboxError> {
            unreachable!()
        }
        async fn broadcast(
            &self,
            _delegation_id: &str,
            _msg: Arc<AgentMessage>,
        ) -> Result<(), MailboxError> {
            unreachable!()
        }
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
    async fn direct_send_requires_canonical_run_identity() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker()));
        let sender = addr("run-sender", "sender");
        let message = AgentMessage::new(
            sender,
            MessageTarget::Direct {
                address: AgentAddress::new("", "worker"),
            },
            MessagePayload::Text {
                content: "hello".into(),
                summary: None,
            },
        );

        assert!(matches!(
            router.send(message).await,
            Err(MailboxError::InvalidAddress(address)) if address.run_id.is_empty()
        ));
    }

    #[tokio::test]
    async fn mailbox_confirms_consumption_only_when_caller_acknowledges() {
        let acknowledged = Arc::new(std::sync::Mutex::new(Vec::new()));
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let mailbox = AgentMailbox {
            address: addr("run-review", "reviewer"),
            delegation_id: None,
            stream: tokio::sync::Mutex::new(Box::new(AckRecordingStream {
                acknowledged: Arc::clone(&acknowledged),
            })),
            buffered: tokio::sync::Mutex::new(VecDeque::new()),
            router,
        };
        let message = Arc::new(AgentMessage::new(
            addr("run-code", "coder"),
            MessageTarget::Direct {
                address: addr("run-review", "reviewer"),
            },
            MessagePayload::Text {
                content: "review this".into(),
                summary: None,
            },
        ));

        assert!(acknowledged.lock().expect("ack recorder lock").is_empty());
        mailbox
            .acknowledge_received(std::slice::from_ref(&message))
            .await
            .unwrap();
        assert_eq!(
            acknowledged.lock().expect("ack recorder lock").as_slice(),
            [message.id.as_str()]
        );
    }

    #[tokio::test]
    async fn mailbox_send_to_parent() {
        let transport = Arc::new(InProcessTransport::new());
        let dt = tracker();
        let router = Arc::new(AgentMailboxRouter::new(transport, dt.clone()));

        let parent = addr("r0", "orchestrator");
        let child = addr("r1", "worker");

        let mut parent_mailbox = router.register(parent.clone(), None).await.unwrap();
        let child_mailbox = router.register(child.clone(), None).await.unwrap();

        dt.record_sub_run(SubRunInfo {
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

    #[tokio::test]
    async fn has_parent_true_for_child() {
        let transport = Arc::new(InProcessTransport::new());
        let dt = tracker();
        let router = Arc::new(AgentMailboxRouter::new(transport, dt.clone()));

        let parent = addr("r0", "orchestrator");
        let child = addr("r1", "worker");

        let _parent_mb = router.register(parent.clone(), None).await.unwrap();
        let child_mb = router.register(child.clone(), None).await.unwrap();

        dt.record_sub_run(SubRunInfo {
            run_id: "r1".into(),
            parent_run_id: "r0".into(),
            delegation_id: "del".into(),
            agent_id: "worker".into(),
            depth: 1,
        })
        .await;

        assert!(child_mb.has_parent().await);
    }

    #[tokio::test]
    async fn has_parent_false_for_root() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker()));

        let root = addr("r0", "orchestrator");
        let root_mb = router.register(root, None).await.unwrap();

        assert!(!root_mb.has_parent().await);
    }

    #[tokio::test]
    async fn agent_resolution_is_scoped_by_delegation() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker()));

        let first = addr("run-a", "worker");
        let second = addr("run-b", "worker");
        let _first_mailbox = router
            .register(first.clone(), Some("delegation-a".into()))
            .await
            .unwrap();
        let _second_mailbox = router
            .register(second.clone(), Some("delegation-b".into()))
            .await
            .unwrap();

        assert_eq!(
            router
                .resolve_agent("delegation-a", "worker")
                .await
                .unwrap(),
            first
        );
        assert_eq!(
            router
                .resolve_agent("delegation-b", "worker")
                .await
                .unwrap(),
            second
        );
        assert_eq!(
            router.list_registered_agents("delegation-a").await.unwrap(),
            vec![first]
        );
    }

    #[tokio::test]
    async fn register_rolls_back_state_when_subscribe_fails() {
        let transport = Arc::new(FailingSubscribeTransport::default());
        let router = Arc::new(AgentMailboxRouter::new(transport.clone(), tracker()));
        let broken = addr("r-broken", "worker");

        let err = match router.register(broken, None).await {
            Ok(_) => panic!("register should fail when subscribe fails"),
            Err(err) => err,
        };
        assert!(matches!(err, MailboxError::Transport(_)));
        assert_eq!(transport.registered.load(AtomicOrdering::Relaxed), 1);
        assert_eq!(transport.unregistered.load(AtomicOrdering::Relaxed), 1);
        assert!(router.address_registry.read().await.is_empty());
    }
}
