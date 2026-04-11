//! Agent mailbox router — transport-agnostic message dispatching.
//!
//! Resolves high-level targets (`Parent`, `Broadcast`) into concrete delivery
//! actions using the delegation tracker and the pluggable transport.

use std::collections::VecDeque;
use std::sync::Arc;

use super::transport::{MessageStream, MessageTransport};
use super::types::{AgentAddress, AgentMessage, MailboxError, MessageTarget};
use crate::server::delegation_engine::{DelegationTracker, SubRunRecord};

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
    /// Returns `Ok(response)` if approved, `Err` with reason if denied or timeout.
    pub async fn request_permission(
        &mut self,
        request: crate::orchestration::permission_sync::PermissionRequest,
        timeout: std::time::Duration,
    ) -> Result<crate::orchestration::permission_sync::PermissionResponse, MailboxError> {
        use crate::messaging::types::{MessagePayload, RequestType};
        use crate::orchestration::permission_sync::PermissionResponse;

        let mut skipped = VecDeque::new();

        // Build and send the request message
        let request_id = uuid::Uuid::new_v4().to_string();
        let msg = AgentMessage::new(
            self.address.clone(),
            MessageTarget::Parent,
            MessagePayload::Request {
                request_type: RequestType::ToolPermission,
                data: serde_json::to_value(&request).unwrap_or_default(),
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
                        if let MessagePayload::Response { data, accepted, .. } = &msg.payload {
                            if let Some(data) = data {
                                if let Some(response) =
                                    PermissionResponse::from_message_payload(data)
                                {
                                    return Ok(response);
                                }
                            }
                            // Fallback: construct response from accepted flag
                            return if *accepted {
                                Ok(PermissionResponse::approve())
                            } else {
                                Ok(PermissionResponse::deny("denied by parent"))
                            };
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
            // Acquire both locks atomically to avoid TOCTOU race between
            // address_registry and agent_id_index.
            let mut reg = self.address_registry.write().await;
            let mut idx = self.agent_id_index.write().await;

            // Insert new address — but first clean up any stale entry for this agent_id
            // to avoid a window where both old and new run_ids coexist.
            if let Some(existing) = idx.get(&addr.agent_id) {
                if existing == &addr {
                    // Already registered with same address — no-op.
                } else {
                    eprintln!(
                        "  ⚠ messaging: agent_id '{}' already registered as {}; overwriting with {}",
                        addr.agent_id, existing, addr
                    );
                    // Clean up stale entry from address_registry (both locks held).
                    reg.remove(&existing.run_id.clone());
                }
            }
            reg.insert(addr.run_id.clone(), addr.clone());
            idx.insert(addr.agent_id.clone(), addr.clone());
        }

        let stream = self.transport.subscribe(&addr).await?;

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
        self.address_registry.write().await.remove(&addr.run_id);
        self.agent_id_index.write().await.remove(&addr.agent_id);
        self.transport.unregister(addr).await
    }

    /// Record a sub-run relationship for parent-target resolution.
    pub async fn record_sub_run(&self, record: SubRunRecord) {
        self.delegation_tracker.record_sub_run(record).await;
    }

    /// Get the known delegation depth for a run.
    pub async fn run_depth(&self, run_id: &str) -> Option<u32> {
        self.delegation_tracker.get_depth(run_id).await
    }

    /// List all registered agent IDs.
    ///
    /// Used by the send_message tool to display broadcast recipients.
    pub async fn list_registered_agents(&self) -> Vec<String> {
        self.agent_id_index.read().await.keys().cloned().collect()
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
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                MailboxError::Transport(format!(
                    "parent run_id '{}' has no agent_id in delegation tracker",
                    parent_run_id
                ))
            })?;

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
        use crate::server::delegation_engine::{SubRunRecord, SubRunState};
        dt.record_sub_run(SubRunRecord {
            run_id: "r1".into(),
            parent_run_id: "r0".into(),
            delegation_id: "del-test".into(),
            agent_id: "worker".into(),
            depth: 1,
            state: SubRunState::Created,
            retry_of: None,
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
    async fn request_permission_approved() {
        use crate::orchestration::permission_sync::{PermissionRequest, PermissionResponse};

        let transport = Arc::new(InProcessTransport::new());
        let dt = tracker();
        let router = Arc::new(AgentMailboxRouter::new(transport, dt.clone()));

        let parent = addr("r0", "orchestrator");
        let child = addr("r1", "worker");

        let mut parent_mailbox = router.register(parent.clone(), None).await.unwrap();
        // Child mailbox will be created in the spawned task

        // Set up parent relationship
        use crate::server::delegation_engine::{SubRunRecord, SubRunState};
        dt.record_sub_run(SubRunRecord {
            run_id: "r1".into(),
            parent_run_id: "r0".into(),
            delegation_id: "del-perm".into(),
            agent_id: "worker".into(),
            depth: 1,
            state: SubRunState::Created,
            retry_of: None,
        })
        .await;

        // Child sends permission request in background, parent responds
        let request =
            PermissionRequest::new("bash", serde_json::json!({"command": "rm -rf /tmp/test"}));
        let timeout = std::time::Duration::from_millis(500);

        // Clone the components needed for the spawned task
        let child_addr = child.clone();
        let child_router = router.clone();

        let child_handle = tokio::spawn(async move {
            // Create a new mailbox for the child in this task
            let mut child_mb = child_router.register(child_addr, None).await.unwrap();
            child_mb.request_permission(request, timeout).await
        });

        // Parent receives request and sends approval
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let received = parent_mailbox.try_recv().expect("should receive request");
        let correlation_id = received.correlation_id.clone().unwrap();

        let response = PermissionResponse::approve();
        let response_msg = AgentMessage::new(
            parent.clone(),
            MessageTarget::Direct {
                address: child.clone(),
            },
            MessagePayload::Response {
                request_id: correlation_id.clone(),
                accepted: true,
                data: Some(serde_json::to_value(&response).unwrap()),
            },
        )
        .with_correlation(&correlation_id);
        router.send(response_msg).await.unwrap();

        // Child should receive approved response
        let result: Result<PermissionResponse, MailboxError> = child_handle.await.unwrap();
        assert!(result.is_ok());
        assert!(result.unwrap().approved);
    }

    #[tokio::test]
    async fn request_permission_timeout() {
        use crate::orchestration::permission_sync::PermissionRequest;

        let transport = Arc::new(InProcessTransport::new());
        let dt = tracker();
        let router = Arc::new(AgentMailboxRouter::new(transport, dt.clone()));

        let parent = addr("r0", "orchestrator");
        let child = addr("r1", "worker");

        let _parent_mailbox = router.register(parent.clone(), None).await.unwrap();
        let mut child_mailbox = router.register(child.clone(), None).await.unwrap();

        // Set up parent relationship
        use crate::server::delegation_engine::{SubRunRecord, SubRunState};
        dt.record_sub_run(SubRunRecord {
            run_id: "r1".into(),
            parent_run_id: "r0".into(),
            delegation_id: "del-timeout".into(),
            agent_id: "worker".into(),
            depth: 1,
            state: SubRunState::Created,
            retry_of: None,
        })
        .await;

        // Child requests permission but parent never responds
        let request = PermissionRequest::new("bash", serde_json::json!({"command": "dangerous"}));
        let timeout = std::time::Duration::from_millis(50);

        let result = child_mailbox.request_permission(request, timeout).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            MailboxError::Timeout(_) => {} // expected
            other => panic!("expected Timeout, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn request_permission_preserves_unrelated_messages() {
        use crate::orchestration::permission_sync::{PermissionRequest, PermissionResponse};
        use crate::server::delegation_engine::SubRunState;

        let transport = Arc::new(InProcessTransport::new());
        let dt = tracker();
        let router = Arc::new(AgentMailboxRouter::new(transport, dt.clone()));

        let parent = addr("r0", "orchestrator");
        let child = addr("r1", "worker");

        let mut parent_mailbox = router.register(parent.clone(), None).await.unwrap();
        let mut child_mailbox = router.register(child.clone(), None).await.unwrap();

        dt.record_sub_run(SubRunRecord {
            run_id: "r1".into(),
            parent_run_id: "r0".into(),
            delegation_id: "del-buffer".into(),
            agent_id: "worker".into(),
            depth: 1,
            state: SubRunState::Created,
            retry_of: None,
        })
        .await;

        let router_clone = router.clone();
        let parent_clone = parent.clone();
        let child_clone = child.clone();
        let responder = tokio::spawn(async move {
            loop {
                if let Some(msg) = parent_mailbox.try_recv() {
                    let correlation_id = msg.correlation_id.clone().unwrap();
                    router_clone
                        .send(AgentMessage::new(
                            parent_clone.clone(),
                            MessageTarget::Direct {
                                address: child_clone.clone(),
                            },
                            MessagePayload::Text {
                                content: "keep this message".into(),
                                summary: None,
                            },
                        ))
                        .await
                        .unwrap();
                    router_clone
                        .send(PermissionResponse::approve().to_message(
                            &parent_clone,
                            &child_clone,
                            &correlation_id,
                        ))
                        .await
                        .unwrap();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });

        let response = child_mailbox
            .request_permission(
                PermissionRequest::new("bash", serde_json::json!({"command": "echo hi"})),
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
        responder.await.unwrap();

        assert!(response.approved);

        let preserved = child_mailbox
            .try_recv()
            .expect("unrelated message should remain buffered");
        match &preserved.payload {
            MessagePayload::Text { content, .. } => assert_eq!(content, "keep this message"),
            other => panic!("expected preserved text message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_permission_rejects_wrong_payload_for_matching_correlation() {
        use crate::orchestration::permission_sync::PermissionRequest;
        use crate::server::delegation_engine::{SubRunRecord, SubRunState};

        let transport = Arc::new(InProcessTransport::new());
        let dt = tracker();
        let router = Arc::new(AgentMailboxRouter::new(transport, dt.clone()));

        let parent = addr("r0", "orchestrator");
        let child = addr("r1", "worker");

        let mut parent_mailbox = router.register(parent.clone(), None).await.unwrap();
        let mut child_mailbox = router.register(child.clone(), None).await.unwrap();

        dt.record_sub_run(SubRunRecord {
            run_id: "r1".into(),
            parent_run_id: "r0".into(),
            delegation_id: "del-protocol".into(),
            agent_id: "worker".into(),
            depth: 1,
            state: SubRunState::Created,
            retry_of: None,
        })
        .await;

        let router_clone = router.clone();
        let parent_clone = parent.clone();
        let child_clone = child.clone();
        let responder = tokio::spawn(async move {
            loop {
                if let Some(msg) = parent_mailbox.try_recv() {
                    let correlation_id = msg.correlation_id.clone().unwrap();
                    router_clone
                        .send(
                            AgentMessage::new(
                                parent_clone.clone(),
                                MessageTarget::Direct {
                                    address: child_clone.clone(),
                                },
                                MessagePayload::Text {
                                    content: "wrong payload".into(),
                                    summary: None,
                                },
                            )
                            .with_correlation(&correlation_id),
                        )
                        .await
                        .unwrap();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });

        let result = child_mailbox
            .request_permission(
                PermissionRequest::new("bash", serde_json::json!({"command": "echo hi"})),
                std::time::Duration::from_secs(1),
            )
            .await;
        responder.await.unwrap();

        match result {
            Err(MailboxError::Protocol(msg)) => {
                assert!(msg.contains("expected response payload"));
            }
            other => panic!("expected protocol error, got {other:?}"),
        }

        assert!(
            child_mailbox.try_recv().is_none(),
            "protocol-error response should not leak into the mailbox buffer"
        );
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

        use crate::server::delegation_engine::{SubRunRecord, SubRunState};
        dt.record_sub_run(SubRunRecord {
            run_id: "r1".into(),
            parent_run_id: "r0".into(),
            delegation_id: "del".into(),
            agent_id: "worker".into(),
            depth: 1,
            state: SubRunState::Created,
            retry_of: None,
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
}
