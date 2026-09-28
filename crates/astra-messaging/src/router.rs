//! Agent mailbox router — transport-agnostic message dispatching.
//!
//! Resolves high-level targets (`Parent`, `Broadcast`) into concrete delivery
//! actions using the delegation tracker and the pluggable transport.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Weak};

use super::delegation::{DelegationLookup, SubRunInfo};
use super::transport::{MailboxSubscription, MessageStream, MessageTransport};
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
    subscription: MailboxSubscription,
    /// Message receive stream (direct + broadcast), mutex-guarded for Sync.
    stream: tokio::sync::Mutex<Box<dyn MessageStream>>,
    /// Messages buffered while waiting for a correlated response.
    buffered: tokio::sync::Mutex<VecDeque<Arc<AgentMessage>>>,
    /// Router reference for sending.
    router: Arc<AgentMailboxRouter>,
}

/// The registration task retains both ownership and its run gate until the
/// caller accepts the mailbox. Abandoning the result releases unread messages
/// through the same path as an ordinary turn ending.
struct RegistrationHandoff {
    mailbox: Option<AgentMailbox>,
    gate: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl RegistrationHandoff {
    fn accept(mut self) -> AgentMailbox {
        let mailbox = self.mailbox.take().expect("registration mailbox");
        self.gate.take();
        mailbox
    }
}

impl Drop for RegistrationHandoff {
    fn drop(&mut self) {
        let (Some(mailbox), Some(held)) = (self.mailbox.take(), self.gate.take()) else {
            return;
        };
        let gate = mailbox.router.registration_gate(&mailbox.address.run_id);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = mailbox.release_unconsumed_owned(gate, Some(held)).await {
                    tracing::warn!(
                        target: "astra_runtime::messaging",
                        error = %error,
                        "abandoned mailbox registration cleanup failed"
                    );
                }
            });
        }
    }
}

/// A process-local receive lease. Dropping an unfinished async consumer puts
/// the exact envelopes back ahead of later arrivals; it never turns a wake
/// hint into delivery authority or a transport acknowledgement.
pub struct MailboxReceiveLease<'a> {
    mailbox: &'a mut AgentMailbox,
    messages: Vec<Arc<AgentMessage>>,
    has_more: bool,
    committed: bool,
}

/// Retain unrelated messages and an unconfirmed response across cancellation
/// of a permission wait. The mailbox remains the sole owner of the stream.
struct PermissionWaitBuffer<'a> {
    buffered: &'a mut VecDeque<Arc<AgentMessage>>,
    skipped: VecDeque<Arc<AgentMessage>>,
    unconfirmed: Option<Arc<AgentMessage>>,
}

impl Drop for PermissionWaitBuffer<'_> {
    fn drop(&mut self) {
        if let Some(message) = self.unconfirmed.take() {
            self.skipped.push_back(message);
        }
        self.skipped.append(self.buffered);
        std::mem::swap(self.buffered, &mut self.skipped);
    }
}

impl MailboxReceiveLease<'_> {
    pub fn messages(&self) -> &[Arc<AgentMessage>] {
        &self.messages
    }

    pub fn mailbox(&self) -> &AgentMailbox {
        self.mailbox
    }

    pub fn has_more(&self) -> bool {
        self.has_more
    }

    /// Release the mailbox's process-local claim only when the caller can
    /// synchronously retain the model input before its next await.
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for MailboxReceiveLease<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let buffered = self.mailbox.buffered.get_mut();
            for message in self.messages.drain(..).rev() {
                buffered.push_front(message);
            }
        }
    }
}

impl AgentMailbox {
    pub fn subscription(&self) -> &MailboxSubscription {
        &self.subscription
    }

    /// Explicit cleanup and Drop present exactly the same original authority.
    pub async fn unregister(&self) -> Result<(), MailboxError> {
        self.router.unregister(&self.subscription).await
    }

    /// End a turn without consuming its late messages. This completion task
    /// retains ownership if the caller is cancelled during async cleanup.
    pub fn release_unconsumed(self) -> impl Future<Output = Result<(), MailboxError>> + Send {
        // Spawn before returning the future. Dropping the awaiter cannot drop
        // an accepted in-process envelope before handoff is complete. Claim
        // the run gate now when free so a new turn cannot overtake handoff.
        let gate = self.router.registration_gate(&self.address.run_id);
        let held = gate.clone().try_lock_owned().ok();
        let task = tokio::spawn(async move { self.release_unconsumed_owned(gate, held).await });
        async move {
            task.await.map_err(|error| {
                MailboxError::Transport(format!("mailbox release task: {error}"))
            })?
        }
    }

    async fn release_unconsumed_owned(
        mut self,
        gate: Arc<tokio::sync::Mutex<()>>,
        held: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<(), MailboxError> {
        let router = Arc::clone(&self.router);
        let run_id = self.address.run_id.clone();
        let _registration = match held {
            Some(held) => held,
            None => gate.lock_owned().await,
        };
        router.transport.unregister(&self.subscription).await?;
        let mut registry = router.address_registry.write().await;
        if registry.get(&run_id) == Some(&self.subscription) {
            registry.remove(&run_id);
        }
        let replacement = registry.get(&run_id).cloned();
        drop(registry);

        if !router.transport.recovers_unacknowledged_on_unregister() {
            // The detached release task owns this mailbox through handoff;
            // caller cancellation cannot strand envelopes in a local Vec.
            // The normal queue limit applies to *new* sends; already-accepted
            // messages must not be discarded while ending a turn.
            let unread = self.drain();
            if !unread.is_empty() {
                let mut retry = VecDeque::new();
                let mut can_forward = replacement.is_some();
                for message in unread {
                    let mut message = (*message).clone();
                    if can_forward && let Some(ref replacement) = replacement {
                        message.to = MessageTarget::Direct {
                            address: replacement.address().clone(),
                        };
                        if router
                            .transport
                            .send(Arc::new(message.clone()))
                            .await
                            .is_ok()
                        {
                            continue;
                        }
                        // Keep the failed envelope and its suffix together;
                        // later sends cannot overtake an earlier failed one.
                        can_forward = false;
                    }
                    retry.push_back(message);
                }
                if !retry.is_empty() {
                    // This is the existing parent backlog, not another
                    // receipt queue. Keep original IDs for later replay.
                    let mut pending = router.pending_parent_messages.lock().await;
                    pending.entry(run_id).or_default().extend(retry);
                }
            }
        }
        Ok(())
    }

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

    /// Resolve an agent inside this mailbox's own delegation namespace.
    ///
    /// Callers should not reach through the mailbox to its router for direct
    /// addressing: the mailbox owns the namespace boundary and transports own
    /// the authoritative lookup implementation.
    pub async fn resolve_delegation_agent(
        &self,
        agent_id: &str,
    ) -> Result<AgentAddress, MailboxError> {
        let delegation_id = self
            .delegation_id
            .as_deref()
            .filter(|delegation_id| !delegation_id.is_empty())
            .ok_or_else(|| {
                MailboxError::Protocol("mailbox is not part of a delegation namespace".to_string())
            })?;
        self.router.resolve_agent(delegation_id, agent_id).await
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

    /// Wait only for transport readiness. The shared loop, not the router,
    /// decides whether the retained message changes model context.
    pub async fn wait_ready(&mut self) -> bool {
        if !self.buffered.get_mut().is_empty() {
            return true;
        }
        let Some(message) = self.stream.get_mut().recv().await else {
            return false;
        };
        // No await after recv: cancellation cannot strand a claimed message.
        self.buffered.get_mut().push_back(message);
        true
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
                self.buffered.get_mut().push_front(extra);
                (msgs, true)
            }
            None => (msgs, false),
        }
    }

    /// Receive for an async consumer without losing envelopes when that
    /// consumer is cancelled during transport or permission I/O.
    pub fn lease_bounded(&mut self, limit: usize) -> MailboxReceiveLease<'_> {
        let (messages, has_more) = self.drain_bounded(limit);
        MailboxReceiveLease {
            mailbox: self,
            messages,
            has_more,
            committed: false,
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
        let mut pending = PermissionWaitBuffer {
            buffered: self.buffered.get_mut(),
            skipped: VecDeque::new(),
            unconfirmed: None,
        };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(MailboxError::Timeout(format!(
                    "Permission request timed out after {:?}",
                    timeout
                )));
            }

            let next_message = if let Some(msg) = pending.buffered.pop_front() {
                Ok(Some(msg))
            } else {
                tokio::time::timeout(remaining, self.stream.lock().await.recv()).await
            };

            match next_message {
                Ok(Some(msg)) => {
                    // Check if this is our response
                    if msg.correlation_id.as_deref() == Some(&request_id) {
                        pending.unconfirmed = Some(Arc::clone(&msg));
                        let outcome = match &msg.payload {
                            MessagePayload::Response { data, accepted, .. } => PermissionOutcome {
                                accepted: *accepted,
                                data: data.clone(),
                            },
                            _ => {
                                return Err(MailboxError::Protocol(format!(
                                    "expected response payload for permission request {request_id}, got {:?}",
                                    msg.payload
                                )));
                            }
                        };
                        self.stream.lock().await.acknowledge(&msg).await?;
                        pending.unconfirmed = None;
                        return Ok(outcome);
                    }
                    pending.skipped.push_back(msg);
                }
                Ok(None) => {
                    return Err(MailboxError::Disconnected);
                }
                Err(_) => {
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
/// Explicit `mailbox.unregister()` calls at usage sites remain the primary
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
        let subscription = self.subscription.clone();
        // Best-effort: spawn only if a tokio runtime is available.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = router.unregister(&subscription).await {
                    tracing::debug!(
                        target: "astra_runtime::messaging",
                        addr = %subscription,
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
    address_registry: tokio::sync::RwLock<std::collections::HashMap<String, MailboxSubscription>>,
    /// Causal/turn run_id → stable mailbox address. Interactive parents can
    /// launch children from a turn-scoped run while receiving their eventual
    /// results through a session-scoped mailbox.
    parent_delivery_aliases: tokio::sync::RwLock<std::collections::HashMap<String, AgentAddress>>,
    /// Match the run-keyed registry and deferred-parent queue, including when
    /// a replacement changes agent labels. Only the same run waits on I/O.
    registration_gates:
        std::sync::Mutex<std::collections::HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    /// Bounded terminal/checkpoint delivery waiting for a temporarily idle
    /// parent mailbox to register again. Direct guidance is never queued here:
    /// a missing child target must remain an explicit rejection.
    pending_parent_messages:
        tokio::sync::Mutex<std::collections::HashMap<String, VecDeque<AgentMessage>>>,
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
            parent_delivery_aliases: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            registration_gates: std::sync::Mutex::new(std::collections::HashMap::new()),
            pending_parent_messages: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn registration_gate(&self, run_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self
            .registration_gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(gate) = gates.get(run_id).and_then(Weak::upgrade) {
            return gate;
        }
        // Keep one identity while an owner or waiter holds it, without
        // retaining a live mutex for every session ever seen by the router.
        gates.retain(|_, gate| gate.strong_count() != 0);
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        gates.insert(run_id.to_string(), Arc::downgrade(&gate));
        gate
    }

    /// Register an agent and return its mailbox handle.
    pub async fn register(
        self: &Arc<Self>,
        addr: AgentAddress,
        delegation_id: Option<String>,
    ) -> Result<AgentMailbox, MailboxError> {
        Ok(self
            .register_owned(addr, delegation_id, false)
            .await?
            .expect("unconditional registration returns a mailbox"))
    }

    async fn register_owned(
        self: &Arc<Self>,
        addr: AgentAddress,
        delegation_id: Option<String>,
        if_absent: bool,
    ) -> Result<Option<AgentMailbox>, MailboxError> {
        let gate = self.registration_gate(&addr.run_id);
        let held = gate.lock_owned().await;
        if if_absent
            && self
                .address_registry
                .read()
                .await
                .contains_key(&addr.run_id)
        {
            return Ok(None);
        }

        // Once transport mutation starts, caller cancellation must not leave
        // a published route without a live consumer. The handoff also owns
        // cleanup if the caller disappears after the task has completed.
        let router = Arc::clone(self);
        let task = tokio::spawn(async move {
            router
                .register_inner(addr, delegation_id)
                .await
                .map(|mailbox| RegistrationHandoff {
                    mailbox: Some(mailbox),
                    gate: Some(held),
                })
        });
        let handoff = task.await.map_err(|error| {
            MailboxError::Transport(format!("mailbox registration task: {error}"))
        })??;
        Ok(Some(handoff.accept()))
    }

    async fn register_inner(
        self: &Arc<Self>,
        addr: AgentAddress,
        delegation_id: Option<String>,
    ) -> Result<AgentMailbox, MailboxError> {
        let subscription = self
            .transport
            .register(addr.clone(), delegation_id.clone())
            .await?;

        let stream = match self.transport.subscribe(&subscription).await {
            Ok(stream) => stream,
            Err(err) => {
                if let Err(unregister_err) = self.transport.unregister(&subscription).await {
                    tracing::warn!(
                        target: "astra_runtime::messaging",
                        addr = %addr,
                        error = ?unregister_err,
                        "failed to roll back transport registration after subscribe error",
                    );
                }
                // The attempted replacement already displaced the old
                // transport owner. Its prior route is no longer usable.
                self.address_registry.write().await.remove(&addr.run_id);
                return Err(err);
            }
        };

        let mut mailbox = AgentMailbox {
            address: addr.clone(),
            delegation_id,
            subscription: subscription.clone(),
            stream: tokio::sync::Mutex::new(stream),
            buffered: tokio::sync::Mutex::new(VecDeque::new()),
            router: Arc::clone(self),
        };
        // Only volatile parents use this backlog. Move original envelopes
        // ahead of new channel arrivals without replay I/O or channel limits.
        let pending = self
            .pending_parent_messages
            .lock()
            .await
            .remove(&addr.run_id)
            .unwrap_or_default();
        for mut message in pending {
            if message.is_expired() {
                continue;
            }
            message.to = MessageTarget::Direct {
                address: addr.clone(),
            };
            mailbox.buffered.get_mut().push_back(Arc::new(message));
        }
        self.address_registry
            .write()
            .await
            .insert(addr.run_id, subscription);
        Ok(mailbox)
    }

    /// Unregister the original subscription (typically on completion/failure).
    /// An address lookup is routing information, never cleanup authority.
    pub async fn unregister(
        self: &Arc<Self>,
        subscription: &MailboxSubscription,
    ) -> Result<(), MailboxError> {
        let gate = self.registration_gate(&subscription.run_id);
        let held = gate.lock_owned().await;
        let router = Arc::clone(self);
        let subscription = subscription.clone();
        let task = tokio::spawn(async move {
            let _registration = held;
            router.transport.unregister(&subscription).await?;
            let mut registry = router.address_registry.write().await;
            if registry.get(&subscription.run_id) == Some(&subscription) {
                registry.remove(&subscription.run_id);
            }
            Ok(())
        });
        task.await
            .map_err(|error| MailboxError::Transport(format!("mailbox cleanup task: {error}")))?
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

    /// Resolve an exact run identity already owned by this router.
    pub async fn registered_address(&self, run_id: &str) -> Option<AgentAddress> {
        self.address_registry
            .read()
            .await
            .get(run_id)
            .map(|s| s.address().clone())
    }

    /// Resolve the sender bound to this execution, never a process-global
    /// agent label shared by unrelated sessions.
    pub async fn sender_address(&self, run_id: &str, agent_id: &str) -> Option<AgentAddress> {
        let registry = self.address_registry.read().await;
        if let Some(subscription) = registry.get(run_id) {
            return (subscription.agent_id == agent_id).then(|| subscription.address().clone());
        }
        drop(registry);
        self.parent_delivery_aliases
            .read()
            .await
            .get(run_id)
            .filter(|address| address.agent_id == agent_id)
            .cloned()
    }

    /// Bind a turn-scoped parent run to the stable mailbox that should receive
    /// its child messages after that turn has settled.
    pub async fn record_parent_delivery_alias(
        &self,
        parent_run_id: &str,
        mailbox_address: &AgentAddress,
    ) {
        if parent_run_id.is_empty()
            || mailbox_address.run_id.is_empty()
            || parent_run_id == mailbox_address.run_id.as_str()
        {
            return;
        }
        self.parent_delivery_aliases
            .write()
            .await
            .insert(parent_run_id.to_string(), mailbox_address.clone());
    }

    /// Return the canonical parent run identity for a child run.
    pub async fn parent_run_id(&self, child_run_id: &str) -> Option<String> {
        self.delegation_tracker.get_parent(child_run_id).await
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
        self.register_owned(addr, delegation_id, true).await
    }

    /// Resolve the address of a parent run.
    async fn resolve_parent_addr(&self, child_run_id: &str) -> Result<AgentAddress, MailboxError> {
        let parent_run_id = self
            .delegation_tracker
            .get_parent(child_run_id)
            .await
            .ok_or(MailboxError::NoParent)?;

        let delivery_address = self
            .parent_delivery_aliases
            .read()
            .await
            .get(&parent_run_id)
            .cloned();
        let delivery_run_id = delivery_address
            .as_ref()
            .map(|address| address.run_id.as_str())
            .unwrap_or(parent_run_id.as_str());

        // Try address registry first (includes root agents and stable aliases).
        if let Some(addr) = self.address_registry.read().await.get(delivery_run_id) {
            return Ok(addr.address().clone());
        }

        if let Some(delivery_address) = delivery_address {
            // The alias remains useful while the stable root mailbox is idle:
            // retain the full canonical identity. Durable transports route by
            // both run_id and agent_id, so synthesizing either field here
            // would persist the message to an address that never registers.
            return Ok(delivery_address);
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
                // A durable direct address includes both run and agent id.
                // Guessing a root label here can report success while a DB
                // transport persists the message for a mailbox that will
                // never register. Reject explicitly and require callers to
                // register/alias the canonical root mailbox first.
                Err(MailboxError::Protocol(format!(
                    "parent run '{parent_run_id}' has no canonical mailbox address (child '{child_run_id}')"
                )))
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
                let parent_run_id = parent_addr.run_id.clone();
                let resolved_msg = AgentMessage {
                    to: MessageTarget::Direct {
                        address: parent_addr,
                    },
                    ..msg
                };
                match self.transport.send(Arc::new(resolved_msg.clone())).await {
                    Ok(()) => Ok(()),
                    Err(MailboxError::AgentNotFound(_)) => {
                        // Close the send-failed → parent-registers → queue-late
                        // race. Registration/unregistration use the same gate:
                        // if registration already won, retry its canonical
                        // address now; if this branch wins, registration will
                        // flush the message after we enqueue it.
                        let gate = self.registration_gate(&parent_run_id);
                        let _registration = gate.lock().await;
                        let current_addr = self
                            .address_registry
                            .read()
                            .await
                            .get(&parent_run_id)
                            .cloned();
                        let queued_message = if let Some(current_addr) = current_addr {
                            let retry_message = AgentMessage {
                                to: MessageTarget::Direct {
                                    address: current_addr.address().clone(),
                                },
                                ..resolved_msg.clone()
                            };
                            match self.transport.send(Arc::new(retry_message.clone())).await {
                                Ok(()) => return Ok(()),
                                Err(MailboxError::AgentNotFound(_)) => retry_message,
                                Err(error) => return Err(error),
                            }
                        } else {
                            resolved_msg
                        };
                        const MAX_PENDING_PARENT_MESSAGES: usize = 256;
                        const MAX_PENDING_PARENT_RUNS: usize = 256;
                        let mut pending = self.pending_parent_messages.lock().await;
                        if !pending.contains_key(&parent_run_id)
                            && pending.len() >= MAX_PENDING_PARENT_RUNS
                        {
                            return Err(MailboxError::Transport(format!(
                                "pending parent mailbox run capacity reached ({MAX_PENDING_PARENT_RUNS}); message was not accepted"
                            )));
                        }
                        let queue = pending.entry(parent_run_id).or_default();
                        if queue.len() >= MAX_PENDING_PARENT_MESSAGES {
                            return Err(MailboxError::Transport(format!(
                                "pending parent mailbox message capacity reached ({MAX_PENDING_PARENT_MESSAGES}); message was not accepted"
                            )));
                        }
                        queue.push_back(queued_message);
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
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
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::task::{Context, Poll, Waker};
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

    struct BlockingAckStream {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl MessageStream for BlockingAckStream {
        async fn recv(&mut self) -> Option<Arc<AgentMessage>> {
            None
        }
        fn try_recv(&mut self) -> Option<Arc<AgentMessage>> {
            None
        }
        async fn acknowledge(&mut self, _message: &AgentMessage) -> Result<(), MailboxError> {
            self.entered.notify_one();
            self.release.notified().await;
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

    #[tokio::test]
    async fn stale_unregister_and_drop_preserve_replacement_mailbox() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let address = addr("same-run", "same-agent");
        let old = router.register(address.clone(), None).await.unwrap();
        let mut replacement = router.register(address.clone(), None).await.unwrap();
        assert_ne!(old.subscription(), replacement.subscription());
        old.unregister().await.unwrap();
        let owners_before_drop = Arc::strong_count(&router);
        drop(old);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while Arc::strong_count(&router) >= owners_before_drop {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("old Drop cleanup finished");
        assert_eq!(
            router.registered_address(&address.run_id).await,
            Some(address.clone())
        );
        let message = AgentMessage::new(
            addr("sender", "sender"),
            MessageTarget::Direct { address },
            MessagePayload::Text {
                content: "replacement is live".into(),
                summary: None,
            },
        );
        let id = message.id.clone();
        router.send(message).await.unwrap();
        assert_eq!(replacement.try_recv().unwrap().id, id);
        replacement.unregister().await.unwrap();
        assert!(!router.is_run_registered("same-run").await);
    }

    #[tokio::test]
    async fn registration_of_another_run_does_not_wait_for_a_held_gate() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let blocked_gate = router.registration_gate("blocked-run");
        let held = blocked_gate.lock().await;

        let other = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            router.register(addr("other-run", "worker"), None),
        )
        .await
        .expect("another run must not wait for this gate")
        .unwrap();
        assert_eq!(other.address.run_id, "other-run");

        let waiting_router = Arc::clone(&router);
        let blocked = tokio::spawn(async move {
            waiting_router
                .register(addr("blocked-run", "worker"), None)
                .await
        });
        tokio::task::yield_now().await;
        blocked.abort();
        let _ = blocked.await;
        drop(held);

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            router.register(addr("blocked-run", "worker"), None),
        )
        .await
        .expect("cancelled waiter must not strand the gate")
        .unwrap();
    }

    #[tokio::test]
    async fn cancelled_registration_releases_accepted_message_and_route() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport.clone(), tracker()));
        let address = addr("run", "root");
        let pending = router.pending_parent_messages.lock().await;
        let registering = {
            let router = router.clone();
            let address = address.clone();
            tokio::spawn(async move { router.register(address, None).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while transport.agent_count().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let message = AgentMessage::new(
            addr("child", "worker"),
            MessageTarget::Direct {
                address: address.clone(),
            },
            MessagePayload::Text {
                content: "accepted while attaching".into(),
                summary: None,
            },
        );
        let id = message.id.clone();
        router.send(message).await.unwrap();
        registering.abort();
        let _ = registering.await;
        drop(pending);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while transport.agent_count().await != 0 || router.is_run_registered("run").await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("abandoned registration must release transport and route");
        let mut replacement = router
            .register_if_absent(address, None)
            .await
            .unwrap()
            .expect("abandoned registration cannot block replacement");
        assert_eq!(replacement.try_recv().unwrap().id, id);
    }

    #[tokio::test]
    async fn abandoned_completed_handoff_releases_original_message() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport.clone(), tracker()));
        let address = addr("run", "root");
        let mailbox = router.register(address.clone(), None).await.unwrap();
        let message = AgentMessage::new(
            addr("child", "worker"),
            MessageTarget::Direct {
                address: address.clone(),
            },
            MessagePayload::Text {
                content: "accepted before handoff".into(),
                summary: None,
            },
        );
        let id = message.id.clone();
        router.send(message).await.unwrap();
        let gate = router.registration_gate(&address.run_id);
        let held = gate.lock_owned().await;
        drop(RegistrationHandoff {
            mailbox: Some(mailbox),
            gate: Some(held),
        });

        let mut next = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            router.register_if_absent(address, None),
        )
        .await
        .expect("handoff cleanup must release the gate")
        .unwrap()
        .expect("abandoned handoff cannot reserve the run");
        assert_eq!(next.try_recv().unwrap().id, id);
    }

    #[tokio::test]
    async fn sender_identity_is_run_scoped_when_sessions_share_agent_label() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let first = addr("session-one", "root-agent");
        let second = addr("session-two", "root-agent");
        let _first_mailbox = router.register(first.clone(), None).await.unwrap();
        let _second_mailbox = router.register(second.clone(), None).await.unwrap();
        router
            .record_parent_delivery_alias("turn-one", &first)
            .await;
        router
            .record_parent_delivery_alias("turn-two", &second)
            .await;

        assert_eq!(
            router.sender_address("turn-one", "root-agent").await,
            Some(first)
        );
        assert_eq!(
            router.sender_address("turn-two", "root-agent").await,
            Some(second)
        );
        assert_eq!(router.sender_address("turn-one", "other-agent").await, None);
        assert_eq!(
            router.sender_address("unknown-turn", "root-agent").await,
            None
        );
    }

    #[tokio::test]
    async fn releasing_volatile_mailbox_preserves_unread_ids_for_next_turn() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport.clone(), tracker()));
        let root = addr("session", "root");
        let sender = addr("child", "worker");
        let mut mailbox = router.register(root.clone(), None).await.unwrap();
        let _sender = router.register(sender.clone(), None).await.unwrap();
        let mut ids = Vec::new();
        for text in ["first", "second", "third"] {
            let message = AgentMessage::new(
                sender.clone(),
                MessageTarget::Direct {
                    address: root.clone(),
                },
                MessagePayload::Text {
                    content: text.into(),
                    summary: None,
                },
            );
            ids.push(message.id.clone());
            router.send(message).await.unwrap();
        }
        assert_eq!(mailbox.try_recv().unwrap().id, ids[0]);
        // A read-ahead envelope and the channel backlog have the same owner.
        assert!(mailbox.wait_ready().await);
        mailbox.release_unconsumed().await.unwrap();

        let sent_before_replay = transport
            .metrics()
            .messages_sent
            .load(AtomicOrdering::Relaxed);
        let mut next = router.register(root.clone(), None).await.unwrap();
        assert_eq!(
            transport
                .metrics()
                .messages_sent
                .load(AtomicOrdering::Relaxed),
            sent_before_replay,
            "volatile backlog ownership transfer must not resend messages"
        );
        let fresh = AgentMessage::new(
            sender,
            MessageTarget::Direct { address: root },
            MessagePayload::Text {
                content: "fresh".into(),
                summary: None,
            },
        );
        let fresh_id = fresh.id.clone();
        router.send(fresh).await.unwrap();
        for expected in [&ids[1], &ids[2], &fresh_id] {
            let lease = next.lease_bounded(1);
            assert_eq!(&lease.messages()[0].id, expected);
            lease.commit();
        }
        assert!(next.try_recv().is_none());
    }

    #[tokio::test]
    async fn cancelling_release_waiter_does_not_discard_accepted_message() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let root = addr("session", "root");
        let sender = addr("child", "worker");
        let mailbox = router.register(root.clone(), None).await.unwrap();
        let message = AgentMessage::new(
            sender,
            MessageTarget::Direct {
                address: root.clone(),
            },
            MessagePayload::Text {
                content: "late answer".into(),
                summary: None,
            },
        );
        let message_id = message.id.clone();
        router.send(message).await.unwrap();

        // The transfer task starts when release_unconsumed is called, before
        // its returned future is polled. Simulate immediate caller cancellation.
        drop(mailbox.release_unconsumed());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while router.is_run_registered(&root.run_id).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached release must finish");
        let mut next = router.register(root, None).await.unwrap();
        assert_eq!(next.try_recv().unwrap().id, message_id);
    }

    #[tokio::test]
    async fn late_old_release_hands_unread_message_to_active_replacement() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let root = addr("session", "root");
        let sender = addr("child", "worker");
        let old = router.register(root.clone(), None).await.unwrap();
        let message = AgentMessage::new(
            sender,
            MessageTarget::Direct {
                address: root.clone(),
            },
            MessagePayload::Text {
                content: "old inbox".into(),
                summary: None,
            },
        );
        let message_id = message.id.clone();
        router.send(message).await.unwrap();

        // A new turn can register before the detached old release wins the
        // run gate. The original message must reach that active receiver.
        let mut next = router.register(root, None).await.unwrap();
        old.release_unconsumed().await.unwrap();
        assert_eq!(next.try_recv().unwrap().id, message_id);
    }

    #[tokio::test]
    async fn next_turn_cannot_overtake_started_mailbox_release() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let root = addr("session", "root");
        let old = router.register(root.clone(), None).await.unwrap();
        let message = AgentMessage::new(
            addr("child", "worker"),
            MessageTarget::Direct {
                address: root.clone(),
            },
            MessagePayload::Text {
                content: "before next turn".into(),
                summary: None,
            },
        );
        let message_id = message.id.clone();
        router.send(message).await.unwrap();

        drop(old.release_unconsumed());
        let mut next = router.register(root, None).await.unwrap();
        assert_eq!(next.try_recv().unwrap().id, message_id);
    }

    fn addr(run: &str, agent: &str) -> AgentAddress {
        AgentAddress::new(run, agent)
    }

    struct FailingSubscribeTransport {
        registered: AtomicUsize,
        unregistered: AtomicUsize,
        subscribe_attempts: AtomicUsize,
        fail_on_attempt: usize,
    }

    impl Default for FailingSubscribeTransport {
        fn default() -> Self {
            Self {
                registered: AtomicUsize::new(0),
                unregistered: AtomicUsize::new(0),
                subscribe_attempts: AtomicUsize::new(0),
                fail_on_attempt: 1,
            }
        }
    }

    #[async_trait]
    impl MessageTransport for FailingSubscribeTransport {
        async fn register(
            &self,
            _addr: AgentAddress,
            _delegation_id: Option<String>,
        ) -> Result<MailboxSubscription, MailboxError> {
            self.registered.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(MailboxSubscription::new(_addr))
        }
        async fn unregister(&self, _addr: &MailboxSubscription) -> Result<(), MailboxError> {
            self.unregistered.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(())
        }
        async fn subscribe(
            &self,
            _addr: &MailboxSubscription,
        ) -> Result<Box<dyn MessageStream>, MailboxError> {
            let attempt = self
                .subscribe_attempts
                .fetch_add(1, AtomicOrdering::Relaxed)
                + 1;
            if attempt == self.fail_on_attempt {
                Err(MailboxError::Transport("subscribe failed".into()))
            } else {
                Ok(Box::new(AckRecordingStream {
                    acknowledged: Arc::new(std::sync::Mutex::new(Vec::new())),
                }))
            }
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
    async fn wait_ready_preserves_message_for_shared_consumer() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker()));
        let sender = addr("child-run", "worker");
        let parent = addr("parent-run", "parent");
        let mut mailbox = router.register(parent.clone(), None).await.unwrap();
        router.register(sender.clone(), None).await.unwrap();
        router
            .send(AgentMessage::new(
                sender.clone(),
                MessageTarget::Direct {
                    address: parent.clone(),
                },
                MessagePayload::Progress {
                    turn_index: 1,
                    tool_calls: 0,
                    status: "working".into(),
                    detail: None,
                },
            ))
            .await
            .unwrap();
        router
            .send(AgentMessage::new(
                sender,
                MessageTarget::Direct { address: parent },
                MessagePayload::Text {
                    content: "need a decision".into(),
                    summary: None,
                },
            ))
            .await
            .unwrap();

        assert!(mailbox.wait_ready().await);
        let progress = mailbox.try_recv().expect("progress stays owned by mailbox");
        assert!(matches!(progress.payload, MessagePayload::Progress { .. }));
        assert!(mailbox.wait_ready().await);
        let received = mailbox.try_recv().expect("text stays owned by mailbox");
        assert!(matches!(
            &received.payload,
            MessagePayload::Text { content, .. } if content == "need a decision"
        ));
        assert!(mailbox.try_recv().is_none());
    }

    #[tokio::test]
    async fn cancelled_permission_wait_restores_unrelated_message() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let parent = addr("parent-run", "parent");
        let child = addr("child-run", "worker");
        let _parent_mailbox = router.register(parent.clone(), None).await.unwrap();
        let mut child_mailbox = router.register(child.clone(), None).await.unwrap();
        router
            .record_sub_run(SubRunInfo {
                run_id: child.run_id.clone(),
                parent_run_id: parent.run_id,
                delegation_id: "delegation".into(),
                agent_id: child.agent_id.clone(),
                depth: 1,
            })
            .await;
        let unrelated = AgentMessage::new(
            addr("peer-run", "peer"),
            MessageTarget::Direct { address: child },
            MessagePayload::Text {
                content: "do not lose this".into(),
                summary: None,
            },
        );
        let id = unrelated.id.clone();
        router.send(unrelated).await.unwrap();

        let mut wait = Box::pin(
            child_mailbox.request_permission("approval", std::time::Duration::from_secs(5)),
        );
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(wait.as_mut().poll(&mut context), Poll::Pending));
        drop(wait);

        assert_eq!(child_mailbox.try_recv().unwrap().id, id);
    }

    #[tokio::test]
    async fn cancelled_receive_lease_restores_original_order() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let sender = addr("sender-run", "sender");
        let receiver = addr("receiver-run", "receiver");
        let mut mailbox = router.register(receiver.clone(), None).await.unwrap();
        router.register(sender.clone(), None).await.unwrap();
        for content in ["first", "second", "third"] {
            router
                .send(AgentMessage::new(
                    sender.clone(),
                    MessageTarget::Direct {
                        address: receiver.clone(),
                    },
                    MessagePayload::Text {
                        content: content.into(),
                        summary: None,
                    },
                ))
                .await
                .unwrap();
        }
        let first_id = {
            let lease = mailbox.lease_bounded(2);
            assert!(lease.has_more());
            lease.messages()[0].id.clone()
            // Simulates dropping a boundary future during an awaited ACK.
        };
        assert_eq!(mailbox.try_recv().unwrap().id, first_id);
        let rest = mailbox.drain();
        let contents = rest
            .iter()
            .map(|message| match &message.payload {
                MessagePayload::Text { content, .. } => content.as_str(),
                _ => panic!("expected text"),
            })
            .collect::<Vec<_>>();
        assert_eq!(contents, ["second", "third"]);
    }

    #[tokio::test]
    async fn cancelled_ack_keeps_claimed_message_in_the_existing_buffer() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let message = Arc::new(AgentMessage::new(
            addr("sender", "sender"),
            MessageTarget::Direct {
                address: addr("receiver", "receiver"),
            },
            MessagePayload::Text {
                content: "question".into(),
                summary: None,
            },
        ));
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let mut mailbox = AgentMailbox {
            subscription: MailboxSubscription::new(addr("receiver", "receiver")),
            address: addr("receiver", "receiver"),
            delegation_id: None,
            stream: tokio::sync::Mutex::new(Box::new(BlockingAckStream {
                entered: entered.clone(),
                release,
            })),
            buffered: tokio::sync::Mutex::new(VecDeque::from([message.clone()])),
            router,
        };
        let lease = mailbox.lease_bounded(1);
        let mut ack = Box::pin(lease.mailbox().acknowledge_received(lease.messages()));
        tokio::select! {
            _ = &mut ack => panic!("ack should remain gated"),
            _ = entered.notified() => {}
        }
        drop(ack);
        drop(lease);
        assert_eq!(mailbox.try_recv().unwrap().id, message.id);
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
            subscription: MailboxSubscription::new(addr("run-review", "reviewer")),
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
        let first_mailbox = router
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
            first_mailbox
                .resolve_delegation_agent("worker")
                .await
                .unwrap(),
            first,
            "mailbox-level resolution must stay inside its delegation namespace"
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

    #[tokio::test]
    async fn failed_replacement_cannot_leave_stale_router_owner() {
        let transport = Arc::new(FailingSubscribeTransport {
            fail_on_attempt: 2,
            ..Default::default()
        });
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker()));
        let address = addr("replaced-run", "worker");
        let old = router.register(address.clone(), None).await.unwrap();
        assert!(router.register(address.clone(), None).await.is_err());
        assert!(!router.is_run_registered(&address.run_id).await);
        let next = router
            .register_if_absent(address, None)
            .await
            .unwrap()
            .expect("old route cannot suppress replacement after subscribe failure");
        assert_ne!(old.subscription(), next.subscription());
    }
}
