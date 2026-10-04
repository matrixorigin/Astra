//! Agent mailbox router — transport-agnostic message dispatching.
//!
//! Resolves high-level targets (`Parent`, `Broadcast`) into concrete delivery
//! actions using the delegation tracker and the pluggable transport.

use std::collections::{HashMap, HashSet, VecDeque};
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
    subscription: MailboxSubscription,
    lifetime: MailboxLifetime,
    /// Message receive stream (direct + broadcast), mutex-guarded for Sync.
    stream: tokio::sync::Mutex<Box<dyn MessageStream>>,
    /// Messages buffered while waiting for a correlated response.
    buffered: tokio::sync::Mutex<VecDeque<Arc<AgentMessage>>>,
    /// Claimed messages that cannot be admitted yet. They remain transport
    /// owned and are invisible to readiness until an explicit retry boundary.
    parked: VecDeque<ParkedDelivery>,
    /// Semantic deliveries staged for the next provider request but not yet
    /// confirmed as consumed. Keeping the exact envelopes with the mailbox
    /// preserves transport custody across turns and redelivery.
    pending_acks: Vec<Arc<AgentMessage>>,
    /// Router reference for sending.
    router: Arc<AgentMailboxRouter>,
    /// Terminal cleanup owns the route; Drop must not start a second cleanup.
    terminal_cleanup_started: bool,
}

struct ParkedDelivery {
    message: Arc<AgentMessage>,
    retry_after_provider: bool,
}

/// The registration task retains both ownership and its run gate until the
/// caller accepts the mailbox. Abandoning the result releases unread messages
/// through the same path as an ordinary turn ending.
struct RegistrationHandoff {
    mailbox: Option<AgentMailbox>,
    gate: Option<tokio::sync::OwnedMutexGuard<()>>,
    fresh_lifetime: bool,
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
        let fresh_lifetime = self.fresh_lifetime;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = mailbox
                    .release_unconsumed_owned(gate, Some(held), fresh_lifetime)
                    .await
                {
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

    /// Keep the exact delivery claimed but remove it from the ready queue.
    /// The mailbox can release it at a later boundary without making later
    /// messages wait behind an envelope that is not currently admissible.
    pub fn defer(mut self, retry_after_provider: bool) {
        self.committed = true;
        for message in self.messages.drain(..) {
            self.mailbox.parked.push_back(ParkedDelivery {
                message,
                retry_after_provider,
            });
        }
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

    pub fn lifetime(&self) -> &MailboxLifetime {
        &self.lifetime
    }

    pub fn registration(&self) -> MailboxRegistration {
        MailboxRegistration {
            lifetime: self.lifetime.clone(),
            subscription: self.subscription.clone(),
        }
    }

    /// Explicit cleanup and Drop present exactly the same original authority.
    pub async fn unregister(&self) -> Result<(), MailboxError> {
        self.router.unregister(&self.subscription).await
    }

    /// End a session or terminal child, rather than merely ending one turn.
    pub fn retire(mut self) -> impl Future<Output = Result<(), MailboxError>> + Send {
        let router = Arc::clone(&self.router);
        let lifetime = self.lifetime.clone();
        self.stream.get_mut().detach();
        self.terminal_cleanup_started = true;
        drop(self);
        router.retire_terminal(&lifetime)
    }

    /// End a turn without consuming its late messages. This completion task
    /// retains ownership if the caller is cancelled during async cleanup.
    pub fn release_unconsumed(self) -> impl Future<Output = Result<(), MailboxError>> + Send {
        // Spawn before returning the future. Dropping the awaiter cannot drop
        // an accepted in-process envelope before handoff is complete. Claim
        // the run gate now when free so a new turn cannot overtake handoff.
        let gate = self.router.registration_gate(&self.address.run_id);
        let held = gate.clone().try_lock_owned().ok();
        let task =
            tokio::spawn(async move { self.release_unconsumed_owned(gate, held, false).await });
        async move {
            task.await.map_err(|error| {
                MailboxError::Transport(format!("mailbox release task: {error}"))
            })?
        }
    }

    async fn release_unconsumed_owned(
        self,
        gate: Arc<tokio::sync::Mutex<()>>,
        held: Option<tokio::sync::OwnedMutexGuard<()>>,
        forget_abandoned: bool,
    ) -> Result<(), MailboxError> {
        let router = Arc::clone(&self.router);
        let run_id = self.address.run_id.clone();
        let subscription = self.subscription.clone();
        let lifetime = self.lifetime.clone();
        let _registration = match held {
            Some(held) => held,
            None => gate.lock_owned().await,
        };
        router.transport.unregister(&self.subscription).await?;
        let mut registry = router.address_registry.write().await;
        if let Some(record) = registry.get_mut(&run_id)
            && record.subscription == self.subscription
        {
            record.attached = false;
        }
        drop(registry);
        // Drop the stream while holding the run gate. Volatile transports
        // restore unacknowledged deliveries to their retained inbox; durable
        // transports release their original claims on unregister.
        drop(self);
        if forget_abandoned
            && router
                .transport
                .forget_abandoned_route(&subscription)
                .await?
        {
            router.retire_owned(&lifetime, _registration).await?;
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

    /// Requeue deliveries deferred until a successful provider boundary.
    /// Permanently inadmissible deliveries stay parked until this mailbox is
    /// detached, so they cannot starve newer messages or spin the loop.
    pub fn retry_deferred(&mut self) {
        let mut retained = VecDeque::new();
        while let Some(parked) = self.parked.pop_front() {
            if parked.retry_after_provider {
                self.buffered.get_mut().push_back(parked.message);
            } else {
                retained.push_back(parked);
            }
        }
        self.parked = retained;
    }

    /// Defer durable confirmation until a provider request has accepted the
    /// staged message. A redelivery with the same logical ID replaces the old
    /// envelope so its current claim token remains the one being confirmed.
    pub fn defer_acknowledgement(&mut self, message: Arc<AgentMessage>) {
        if let Some(existing) = self
            .pending_acks
            .iter_mut()
            .find(|pending| pending.id == message.id)
        {
            *existing = message;
        } else {
            self.pending_acks.push(message);
        }
    }

    /// Take the deliveries whose staged context crossed the provider boundary.
    pub fn take_adopted_acknowledgements(&mut self) -> Vec<Arc<AgentMessage>> {
        std::mem::take(&mut self.pending_acks)
    }

    /// Restore only confirmations that failed, preserving the current envelope
    /// if the transport redelivered the same logical message meanwhile.
    pub fn restore_adopted_acknowledgements(&mut self, messages: Vec<Arc<AgentMessage>>) {
        for message in messages {
            self.defer_acknowledgement(message);
        }
    }

    /// Confirm durable consumption after the caller has converted the messages
    /// into runtime state. A failed confirmation leaves the transport claim
    /// recoverable for redelivery instead of silently losing the message.
    pub async fn acknowledge_received(
        &self,
        messages: &[Arc<AgentMessage>],
    ) -> Result<(), MailboxError> {
        let (_, first_error) = self.acknowledge_received_with_failures(messages).await;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Confirm a batch and return only deliveries that still need retry.
    /// Streams may acknowledge some entries before a later entry fails, so a
    /// caller must never retry the original batch wholesale.
    pub async fn acknowledge_received_with_failures(
        &self,
        messages: &[Arc<AgentMessage>],
    ) -> (Vec<Arc<AgentMessage>>, Option<MailboxError>) {
        let mut stream = self.stream.lock().await;
        let mut failures = Vec::new();
        let mut first_error = None;
        for (message, outcome) in messages.iter().zip(stream.acknowledge_many(messages).await) {
            if let Err(error) = outcome {
                if first_error.is_none() {
                    first_error = Some(error);
                }
                failures.push(Arc::clone(message));
            }
        }
        (failures, first_error)
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

        let expected_responder = self
            .router
            .resolve_parent_addr(&self.address.run_id)
            .await?;
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
                    if msg.correlation_id.as_deref() == Some(&request_id)
                        && msg.from == expected_responder
                    {
                        pending.unconfirmed = Some(Arc::clone(&msg));
                        let outcome = match &msg.payload {
                            MessagePayload::Response {
                                request_id: reply_id,
                                data,
                                accepted,
                            } if reply_id == &request_id => PermissionOutcome {
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

/// Detach this turn's receiver without ending its mailbox lifetime.
/// Terminal session/child settlement must explicitly retire the lifetime;
/// otherwise late messages accepted between turns could be lost.
///
/// Uses `tokio::task::spawn` because `unregister` is async and `Drop` is sync.
/// The spawned task is fire-and-forget — if the runtime is shutting down,
/// the unregister may not complete, but that's acceptable since the transport
/// is being torn down anyway.
impl Drop for AgentMailbox {
    fn drop(&mut self) {
        self.stream.get_mut().detach();
        if self.terminal_cleanup_started {
            return;
        }
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

/// Index aliases by canonical address so turn reattachment and terminal
/// retirement touch only one session's turns, not every active session.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MailboxLifetime {
    address: AgentAddress,
    id: String,
}

impl MailboxLifetime {
    fn new(address: AgentAddress) -> Self {
        Self {
            address,
            id: uuid::Uuid::new_v4().simple().to_string(),
        }
    }

    pub fn address(&self) -> &AgentAddress {
        &self.address
    }
}

impl std::ops::Deref for MailboxLifetime {
    type Target = AgentAddress;

    fn deref(&self) -> &Self::Target {
        &self.address
    }
}

/// The lifecycle's stable mailbox identity and this execution's attachment
/// token travel together. A later attachment may replace the token without
/// changing the lifetime that terminal settlement must retire.
#[derive(Clone, Debug)]
pub struct MailboxRegistration {
    lifetime: MailboxLifetime,
    subscription: MailboxSubscription,
}

impl MailboxRegistration {
    pub fn lifetime(&self) -> &MailboxLifetime {
        &self.lifetime
    }

    pub fn subscription(&self) -> &MailboxSubscription {
        &self.subscription
    }

    pub fn address(&self) -> &AgentAddress {
        self.lifetime.address()
    }
}

impl std::ops::Deref for MailboxRegistration {
    type Target = AgentAddress;

    fn deref(&self) -> &Self::Target {
        self.address()
    }
}

impl std::fmt::Display for MailboxRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.address().fmt(f)
    }
}

#[derive(Clone)]
struct MailboxRecord {
    lifetime: MailboxLifetime,
    subscription: MailboxSubscription,
    attached: bool,
}

#[derive(Default)]
struct ParentDeliveryAliases {
    by_turn: HashMap<String, ParentDeliveryAlias>,
    by_address: HashMap<AgentAddress, HashSet<String>>,
}

#[derive(Clone)]
struct ParentDeliveryAlias {
    lifetime: MailboxLifetime,
    sender_agent_id: String,
}

impl ParentDeliveryAliases {
    fn get(&self, turn: &str) -> Option<&ParentDeliveryAlias> {
        self.by_turn.get(turn)
    }

    fn insert(&mut self, turn: String, owner: MailboxLifetime, sender_agent_id: String) {
        if let Some(previous) = self.by_turn.insert(
            turn.clone(),
            ParentDeliveryAlias {
                lifetime: owner.clone(),
                sender_agent_id,
            },
        ) {
            let previous_address = previous.lifetime.address();
            if let Some(turns) = self.by_address.get_mut(previous_address) {
                turns.remove(&turn);
                if turns.is_empty() {
                    self.by_address.remove(previous_address);
                }
            }
        }
        self.by_address
            .entry(owner.address().clone())
            .or_default()
            .insert(turn);
    }

    fn retire(&mut self, owner: &MailboxLifetime) {
        let by_turn = &mut self.by_turn;
        if let Some(turns) = self.by_address.get_mut(owner.address()) {
            turns.retain(|turn| {
                if by_turn
                    .get(turn)
                    .is_some_and(|alias| &alias.lifetime == owner)
                {
                    by_turn.remove(turn);
                    false
                } else {
                    true
                }
            });
            if turns.is_empty() {
                self.by_address.remove(owner.address());
            }
        }
    }
}

/// Central message router that resolves targets and dispatches via a transport.
pub struct AgentMailboxRouter {
    transport: Arc<dyn MessageTransport>,
    delegation_tracker: Arc<dyn DelegationLookup>,
    /// One canonical lifetime per run; detach only clears its receiver attachment.
    address_registry: tokio::sync::RwLock<std::collections::HashMap<String, MailboxRecord>>,
    /// Causal/turn run_id → stable mailbox address. Interactive parents can
    /// launch children from a turn-scoped run while receiving their eventual
    /// results through a session-scoped mailbox.
    parent_delivery_aliases: tokio::sync::RwLock<ParentDeliveryAliases>,
    /// Match the run-keyed registry, including when
    /// a replacement changes agent labels. Only the same run waits on I/O.
    registration_gates:
        std::sync::Mutex<std::collections::HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
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
            parent_delivery_aliases: tokio::sync::RwLock::new(ParentDeliveryAliases::default()),
            registration_gates: std::sync::Mutex::new(std::collections::HashMap::new()),
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
        let gate = self.registration_gate(&addr.run_id);
        let held = gate.lock_owned().await;

        // Once transport mutation starts, caller cancellation must not leave
        // a published route without a live consumer. The handoff also owns
        // cleanup if the caller disappears after the task has completed.
        let router = Arc::clone(self);
        let task = tokio::spawn(async move {
            router
                .register_inner(addr, delegation_id)
                .await
                .map(|(mailbox, fresh_lifetime)| RegistrationHandoff {
                    mailbox: Some(mailbox),
                    gate: Some(held),
                    fresh_lifetime,
                })
        });
        let handoff = task.await.map_err(|error| {
            MailboxError::Transport(format!("mailbox registration task: {error}"))
        })??;
        Ok(handoff.accept())
    }

    async fn register_inner(
        self: &Arc<Self>,
        addr: AgentAddress,
        delegation_id: Option<String>,
    ) -> Result<(AgentMailbox, bool), MailboxError> {
        let previous = self
            .address_registry
            .read()
            .await
            .get(&addr.run_id)
            .cloned();
        if let Some(record) = &previous
            && record.lifetime.address() != &addr
        {
            return Err(MailboxError::Protocol(format!(
                "run '{}' is already bound to another mailbox address",
                addr.run_id
            )));
        }
        let subscription = self.transport.register(addr.clone(), delegation_id).await?;

        let stream = match self.transport.subscribe(&subscription).await {
            Ok(stream) => stream,
            Err(err) => {
                let unregistered = self.transport.unregister(&subscription).await;
                if let Err(unregister_err) = &unregistered {
                    tracing::warn!(
                        target: "astra_runtime::messaging",
                        addr = %addr,
                        error = ?unregister_err,
                        "failed to roll back transport registration after subscribe error",
                    );
                }
                if let Some(mut previous) = previous {
                    // The transport may have replaced a detached attachment
                    // before subscribe failed. Keep the same lifetime but use
                    // its latest token for eventual terminal retirement.
                    previous.subscription = subscription;
                    previous.attached = false;
                    self.address_registry
                        .write()
                        .await
                        .insert(addr.run_id.clone(), previous);
                } else if unregistered.is_ok()
                    && let Err(error) = self.transport.forget_abandoned_route(&subscription).await
                {
                    tracing::warn!(
                        target: "astra_runtime::messaging",
                        addr = %addr,
                        %error,
                        "failed to reclaim empty failed mailbox registration"
                    );
                }
                return Err(err);
            }
        };

        let fresh_lifetime = previous.is_none();
        let lifetime = previous
            .map(|record| record.lifetime)
            .unwrap_or_else(|| MailboxLifetime::new(addr.clone()));

        let mailbox = AgentMailbox {
            address: addr.clone(),
            subscription: subscription.clone(),
            lifetime: lifetime.clone(),
            stream: tokio::sync::Mutex::new(stream),
            buffered: tokio::sync::Mutex::new(VecDeque::new()),
            parked: VecDeque::new(),
            pending_acks: Vec::new(),
            router: Arc::clone(self),
            terminal_cleanup_started: false,
        };
        self.address_registry.write().await.insert(
            addr.run_id.clone(),
            MailboxRecord {
                lifetime,
                subscription,
                attached: true,
            },
        );
        Ok((mailbox, fresh_lifetime))
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
            let attached = router
                .address_registry
                .read()
                .await
                .get(&subscription.run_id)
                .is_some_and(|record| record.attached && record.subscription == subscription);
            if !attached {
                return Ok(());
            }
            router.transport.unregister(&subscription).await?;
            let mut registry = router.address_registry.write().await;
            if let Some(record) = registry.get_mut(&subscription.run_id)
                && record.subscription == subscription
            {
                record.attached = false;
            }
            Ok(())
        });
        task.await
            .map_err(|error| MailboxError::Transport(format!("mailbox cleanup task: {error}")))?
    }

    async fn retire_owned(
        &self,
        lifetime: &MailboxLifetime,
        _held: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), MailboxError> {
        let Some(record) = self
            .address_registry
            .read()
            .await
            .get(&lifetime.run_id)
            .filter(|record| record.lifetime == *lifetime)
            .cloned()
        else {
            return Ok(());
        };
        if record.attached {
            self.transport.unregister(&record.subscription).await?;
        }
        self.transport.retire(&record.subscription).await?;
        let mut registry = self.address_registry.write().await;
        if registry
            .get(&lifetime.run_id)
            .is_some_and(|current| current.lifetime == *lifetime)
        {
            registry.remove(&lifetime.run_id);
        }
        drop(registry);
        self.parent_delivery_aliases.write().await.retire(lifetime);
        Ok(())
    }

    /// End a terminal mailbox lifetime and its aliases. Cleanup starts before
    /// the returned future is polled, so cancelling a timeout cannot split
    /// unregister from retirement. A different lifetime is fenced by the run gate.
    pub fn retire_terminal(
        self: &Arc<Self>,
        lifetime: &MailboxLifetime,
    ) -> impl Future<Output = Result<(), MailboxError>> + Send + use<> {
        let router = Arc::clone(self);
        let lifetime = lifetime.clone();
        let task = tokio::spawn(async move {
            let held = router
                .registration_gate(&lifetime.run_id)
                .lock_owned()
                .await;
            let result = router.retire_owned(&lifetime, held).await;
            if let Err(error) = &result {
                tracing::warn!(
                    target: "astra_runtime::messaging",
                    run_id = %lifetime.run_id,
                    %error,
                    "terminal mailbox cleanup failed"
                );
            }
            result
        });
        async move {
            task.await.map_err(|error| {
                MailboxError::Transport(format!("mailbox terminal cleanup task: {error}"))
            })?
        }
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

    /// Resolve an exact run identity whose mailbox lifetime has not retired.
    /// A detached run still owns its retained inbox and can be addressed.
    pub async fn registered_address(&self, run_id: &str) -> Option<AgentAddress> {
        self.address_registry
            .read()
            .await
            .get(run_id)
            .map(|record| record.lifetime.address().clone())
    }

    /// Resolve the sender bound to this execution, never a process-global
    /// agent label shared by unrelated sessions.
    pub async fn sender_address(&self, run_id: &str, agent_id: &str) -> Option<AgentAddress> {
        if let Some(alias) = self.parent_delivery_aliases.read().await.get(run_id) {
            return (alias.sender_agent_id == agent_id).then(|| alias.lifetime.address().clone());
        }
        self.address_registry
            .read()
            .await
            .get(run_id)
            .filter(|record| record.lifetime.agent_id == agent_id)
            .map(|record| record.lifetime.address().clone())
    }

    /// Bind a turn-scoped parent run to the stable mailbox that should receive
    /// its child messages after that turn has settled.
    pub async fn record_parent_delivery_alias(
        &self,
        parent_run_id: &str,
        mailbox_address: &AgentAddress,
        sender_agent_id: &str,
    ) {
        if parent_run_id.is_empty()
            || mailbox_address.run_id.is_empty()
            || sender_agent_id.is_empty()
            || parent_run_id == mailbox_address.run_id.as_str()
        {
            return;
        }
        let gate = self.registration_gate(&mailbox_address.run_id);
        let _registration = gate.lock().await;
        let owner = self
            .address_registry
            .read()
            .await
            .get(&mailbox_address.run_id)
            .filter(|record| record.lifetime.address() == mailbox_address)
            .map(|record| record.lifetime.clone());
        if let Some(owner) = owner {
            self.parent_delivery_aliases.write().await.insert(
                parent_run_id.to_string(),
                owner,
                sender_agent_id.to_string(),
            );
        }
    }

    /// Return the canonical parent run identity for a child run.
    pub async fn parent_run_id(&self, child_run_id: &str) -> Option<String> {
        self.delegation_tracker.get_parent(child_run_id).await
    }

    /// Check whether a specific run_id is registered in the address registry.
    pub async fn is_run_registered(&self, run_id: &str) -> bool {
        self.address_registry
            .read()
            .await
            .get(run_id)
            .is_some_and(|record| record.attached)
    }

    /// Resolve the address of a parent run.
    async fn resolve_parent_addr(&self, child_run_id: &str) -> Result<AgentAddress, MailboxError> {
        let parent_run_id = self
            .delegation_tracker
            .get_parent(child_run_id)
            .await
            .ok_or(MailboxError::NoParent)?;

        let aliased_lifetime = self
            .parent_delivery_aliases
            .read()
            .await
            .get(&parent_run_id)
            .map(|alias| alias.lifetime.clone());
        let delivery_run_id = aliased_lifetime
            .as_ref()
            .map(|lifetime| lifetime.run_id.as_str())
            .unwrap_or(parent_run_id.as_str());

        // Both attached and detached adopted lifetimes are valid destinations.
        // A stale alias must never route into a later lifetime at the same
        // address, nor synthesize a durable queue for an unknown consumer.
        if let Some(record) = self.address_registry.read().await.get(delivery_run_id)
            && aliased_lifetime
                .as_ref()
                .is_none_or(|alias| alias == &record.lifetime)
        {
            return Ok(record.lifetime.address().clone());
        }

        // A tracker relationship proves lineage, not an active or resumable
        // consumer. Guessing an address would let a durable transport accept
        // messages for a mailbox that nobody ever registered.
        Err(MailboxError::Protocol(format!(
            "parent run '{parent_run_id}' has no canonical mailbox address (child '{child_run_id}')"
        )))
    }

    /// Resolve the user-facing recipient once, before an envelope is built.
    ///
    /// The returned target is canonical: parent and named recipients become
    /// direct addresses, so [`Self::send`] does not repeat hierarchy or
    /// mailbox lookups. The router owns this boundary because it is the only
    /// component that has both transport addresses and delegation lineage.
    pub async fn resolve_message_target(
        &self,
        sender_run_id: &str,
        recipient: &str,
    ) -> Result<(MessageTarget, String, Option<Vec<String>>), MailboxError> {
        let recipient = recipient.trim();
        if recipient.is_empty() {
            return Err(MailboxError::Protocol(
                "send_message requires a non-empty `to`".into(),
            ));
        }

        match recipient.to_ascii_lowercase().as_str() {
            "parent" | "orchestrator" => {
                let address = self.resolve_parent_addr(sender_run_id).await?;
                return Ok((MessageTarget::Direct { address }, "parent".into(), None));
            }
            "*" | "broadcast" | "all" | "peers" => {
                let namespace = self
                    .parent_run_id(sender_run_id)
                    .await
                    .unwrap_or_else(|| sender_run_id.to_string());
                let addresses = self.list_registered_agents(&namespace).await?;
                let recipients = addresses
                    .iter()
                    .filter(|address| address.run_id != sender_run_id)
                    .map(|address| address.agent_id.clone())
                    .collect::<Vec<_>>();
                if recipients.is_empty() {
                    return Err(MailboxError::Protocol(
                        "no active peer agents are available for broadcast".into(),
                    ));
                }
                return Ok((
                    MessageTarget::Broadcast {
                        delegation_id: namespace,
                    },
                    "broadcast".into(),
                    Some(recipients),
                ));
            }
            _ => {}
        }

        if let Some(address) = self.registered_address(recipient).await {
            let sender_parent = self.parent_run_id(sender_run_id).await;
            let target_parent = self.parent_run_id(&address.run_id).await;
            let related = target_parent.as_deref() == Some(sender_run_id)
                || sender_parent.as_deref() == Some(address.run_id.as_str())
                || sender_parent.is_some() && sender_parent == target_parent;
            if !related {
                return Err(MailboxError::Protocol(format!(
                    "target run_id '{recipient}' is outside the sender's parent/child/peer delegation boundary"
                )));
            }
            let display = address.to_string();
            return Ok((MessageTarget::Direct { address }, display, None));
        }

        if let Ok(address) = self.resolve_agent(sender_run_id, recipient).await {
            let display = address.to_string();
            return Ok((MessageTarget::Direct { address }, display, None));
        }
        if let Some(parent_run_id) = self.parent_run_id(sender_run_id).await
            && let Ok(address) = self.resolve_agent(&parent_run_id, recipient).await
        {
            let display = address.to_string();
            return Ok((MessageTarget::Direct { address }, display, None));
        }

        Err(MailboxError::Protocol(format!(
            "target '{recipient}' is not an active child, peer, or exact run_id"
        )))
    }

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

    struct SelectiveAckStream {
        failing_id: String,
        acknowledged: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl MessageStream for SelectiveAckStream {
        async fn recv(&mut self) -> Option<Arc<AgentMessage>> {
            None
        }

        fn try_recv(&mut self) -> Option<Arc<AgentMessage>> {
            None
        }

        async fn acknowledge(&mut self, message: &AgentMessage) -> Result<(), MailboxError> {
            if message.id == self.failing_id {
                return Err(MailboxError::Transport("synthetic ACK failure".into()));
            }
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
    async fn resolved_message_display_uses_the_canonical_agent_address() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let parent = router
            .register(addr("parent-run", "root"), None)
            .await
            .unwrap();
        let child_address = addr("child-run", "worker@child-run");
        let _child = router
            .register(child_address.clone(), Some("parent-run".into()))
            .await
            .unwrap();

        let (_, display, _) = router
            .resolve_message_target("parent-run", &child_address.agent_id)
            .await
            .unwrap();

        assert_eq!(display, "worker@child-run");
        assert!(!display.contains("@child-run@child-run"));
        parent.retire().await.unwrap();
    }

    #[tokio::test]
    async fn active_mailbox_rejects_replacement_and_stale_cleanup_preserves_successor() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let address = addr("same-run", "same-agent");
        let old = router.register(address.clone(), None).await.unwrap();
        assert!(router.register(address.clone(), None).await.is_err());
        old.unregister().await.unwrap();
        drop(old);
        let mut replacement = router.register(address.clone(), None).await.unwrap();
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
    async fn cancelled_terminal_waiter_still_retires_address_and_alias() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport.clone(), tracker()));
        let address = addr("session", "root");
        let mailbox = router.register(address.clone(), None).await.unwrap();
        router
            .record_parent_delivery_alias("prior-turn", &address, &address.agent_id)
            .await;
        let gate = router.registration_gate(&address.run_id);
        let held = gate.lock_owned().await;
        let cleanup = mailbox.retire();
        drop(cleanup);
        drop(held);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if router.sender_address("prior-turn", "root").await.is_none()
                    && transport.agent_count().await == 0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal cleanup survives cancellation");
        let late = Arc::new(AgentMessage::new(
            addr("child", "worker"),
            MessageTarget::Direct { address },
            MessagePayload::Text {
                content: "late".into(),
                summary: None,
            },
        ));
        assert!(transport.send(late).await.is_err());
    }

    #[tokio::test]
    async fn stale_terminal_cleanup_keeps_successor_alias() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker()));
        let address = addr("session", "root");
        let old = router.register(address.clone(), None).await.unwrap();
        let old_lifetime = old.lifetime().clone();
        old.retire().await.unwrap();
        let replacement = router.register(address.clone(), None).await.unwrap();
        router
            .record_parent_delivery_alias("new-turn", &address, &address.agent_id)
            .await;
        router.retire_terminal(&old_lifetime).await.unwrap();
        assert_eq!(
            router.sender_address("new-turn", "root").await,
            Some(address)
        );
        drop(replacement);
    }

    #[tokio::test]
    async fn terminal_children_reclaim_capacity_with_a_thousand_live_sessions() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport.clone(), tracker()));
        let mut roots = Vec::with_capacity(1_000);
        for index in 0..1_000 {
            roots.push(
                router
                    .register(addr(&format!("session-{index}"), "root"), None)
                    .await
                    .unwrap(),
            );
        }
        for index in 0..8_200 {
            let child = router
                .register(addr(&format!("child-{index}"), "worker"), None)
                .await
                .unwrap();
            child.retire().await.unwrap();
        }
        assert_eq!(transport.retained_inbox_count().await, roots.len());
        for root in roots {
            root.retire().await.unwrap();
        }
        assert_eq!(transport.retained_inbox_count().await, 0);
    }

    #[tokio::test]
    async fn terminal_successor_retires_aliases_from_prior_turns() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker()));
        let address = addr("session", "root");
        let first = router.register(address.clone(), None).await.unwrap();
        router
            .record_parent_delivery_alias("turn-one", &address, &address.agent_id)
            .await;
        first.release_unconsumed().await.unwrap();
        let second = router.register(address.clone(), None).await.unwrap();
        router
            .record_parent_delivery_alias("turn-two", &address, &address.agent_id)
            .await;
        assert_eq!(
            router.sender_address("turn-one", "root").await,
            Some(address.clone())
        );
        second.retire().await.unwrap();
        assert!(router.sender_address("turn-one", "root").await.is_none());
        assert!(router.sender_address("turn-two", "root").await.is_none());
    }

    #[tokio::test]
    async fn different_turn_profiles_share_one_mailbox_without_sender_spoofing() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let stable = addr("session", "root-agent");
        let first = router.register(stable.clone(), None).await.unwrap();
        router
            .record_parent_delivery_alias("turn-plan", &stable, "planner")
            .await;
        first.release_unconsumed().await.unwrap();
        let second = router.register(stable.clone(), None).await.unwrap();
        router
            .record_parent_delivery_alias("turn-review", &stable, "reviewer")
            .await;
        assert_eq!(
            router.sender_address("turn-plan", "planner").await,
            Some(stable.clone())
        );
        assert_eq!(
            router.sender_address("turn-review", "reviewer").await,
            Some(stable)
        );
        assert!(
            router
                .sender_address("turn-plan", "reviewer")
                .await
                .is_none()
        );
        assert!(
            router
                .sender_address("turn-review", "planner")
                .await
                .is_none()
        );
        second.retire().await.unwrap();
    }

    #[tokio::test]
    async fn detached_nested_parent_retains_canonical_route_until_terminal() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let address = addr("nested-parent", "planner");
        let parent = router.register(address.clone(), None).await.unwrap();
        router
            .record_sub_run(SubRunInfo {
                run_id: "nested-child".into(),
                parent_run_id: address.run_id.clone(),
                delegation_id: "group".into(),
                agent_id: "worker".into(),
                depth: 2,
            })
            .await;
        parent.release_unconsumed().await.unwrap();

        let message = AgentMessage::new(
            addr("nested-child", "worker"),
            MessageTarget::Parent,
            MessagePayload::Text {
                content: "need guidance".into(),
                summary: None,
            },
        );
        let id = message.id.clone();
        router.send(message).await.unwrap();

        let mut resumed = router.register(address, None).await.unwrap();
        assert_eq!(resumed.try_recv().unwrap().id, id);
        resumed.retire().await.unwrap();
    }

    #[tokio::test]
    async fn detached_child_accepts_parent_answer_until_terminal_retirement() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let parent_address = addr("parent-run", "orchestrator");
        let child_address = addr("child-run", "worker");
        let parent = router.register(parent_address.clone(), None).await.unwrap();
        let child = router.register(child_address.clone(), None).await.unwrap();
        router
            .record_sub_run(SubRunInfo {
                run_id: child_address.run_id.clone(),
                parent_run_id: parent_address.run_id.clone(),
                delegation_id: parent_address.run_id.clone(),
                agent_id: child_address.agent_id.clone(),
                depth: 1,
            })
            .await;
        child.release_unconsumed().await.unwrap();
        assert_eq!(
            router.registered_address(&child_address.run_id).await,
            Some(child_address.clone())
        );
        let answer = AgentMessage::new(
            parent_address,
            MessageTarget::Direct {
                address: child_address.clone(),
            },
            MessagePayload::Response {
                request_id: "question-1".into(),
                accepted: true,
                data: Some(serde_json::json!({"content": "Use JSON."})),
            },
        );
        let answer_id = answer.id.clone();
        router.send(answer.clone()).await.unwrap();
        let mut resumed = router.register(child_address.clone(), None).await.unwrap();
        assert_eq!(resumed.try_recv().unwrap().id, answer_id);
        resumed.retire().await.unwrap();
        assert!(
            router
                .registered_address(&child_address.run_id)
                .await
                .is_none()
        );
        assert!(router.send(answer).await.is_err());
        parent.retire().await.unwrap();
    }

    #[tokio::test]
    async fn session_alias_precedes_a_turn_address_for_sender_identity() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let stable = addr("session", "root");
        let _session = router.register(stable.clone(), None).await.unwrap();
        router
            .record_parent_delivery_alias("turn", &stable, &stable.agent_id)
            .await;
        let _obsolete_turn = router.register(addr("turn", "root"), None).await.unwrap();
        assert_eq!(router.sender_address("turn", "root").await, Some(stable));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_mailbox_immediately_allows_same_address_to_attach() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let address = addr("session", "root");
        let old = router.register(address.clone(), None).await.unwrap();
        let message = AgentMessage::new(
            addr("child", "worker"),
            MessageTarget::Direct {
                address: address.clone(),
            },
            MessagePayload::Text {
                content: "accepted".into(),
                summary: None,
            },
        );
        let id = message.id.clone();
        router.send(message).await.unwrap();
        drop(old);
        let mut next = router.register(address, None).await.unwrap();
        assert_eq!(next.try_recv().unwrap().id, id);
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
    async fn cancelled_new_registration_reclaims_empty_provisional_inbox() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport.clone(), tracker()));
        let address = addr("empty-run", "root");
        let registry = router.address_registry.read().await;
        let registering = {
            let router = Arc::clone(&router);
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
        registering.abort();
        let _ = registering.await;
        drop(registry);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while transport.retained_inbox_count().await != 0
                || router.registered_address("empty-run").await.is_some()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("empty provisional inbox must release its bounded address slot");
    }

    #[tokio::test]
    async fn cancelled_new_registration_retains_accepted_message_for_retry() {
        let transport = Arc::new(InProcessTransport::new());
        let router = Arc::new(AgentMailboxRouter::new(transport.clone(), tracker()));
        let address = addr("run", "root");
        let registry = router.address_registry.read().await;
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
        let message_id = message.id.clone();
        router.send(message).await.unwrap();
        registering.abort();
        let _ = registering.await;
        drop(registry);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while transport.agent_count().await != 0 || router.is_run_registered("run").await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("abandoned registration must release transport and route");
        let mut replacement = router.register(address, None).await.unwrap();
        assert_eq!(replacement.try_recv().unwrap().id, message_id);
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
            fresh_lifetime: false,
        });

        let mut next = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            router.register(address, None),
        )
        .await
        .expect("handoff cleanup must release the gate")
        .unwrap();
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
            .record_parent_delivery_alias("turn-one", &first, &first.agent_id)
            .await;
        router
            .record_parent_delivery_alias("turn-two", &second, &second.agent_id)
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
        let consumed = mailbox.try_recv().unwrap();
        assert_eq!(consumed.id, ids[0]);
        mailbox.acknowledge_received(&[consumed]).await.unwrap();
        // A read-ahead envelope and the channel backlog have the same owner.
        assert!(mailbox.wait_ready().await);
        mailbox.release_unconsumed().await.unwrap();

        let mut next = router.register(root.clone(), None).await.unwrap();
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
    async fn parent_messages_accepted_while_idle_keep_order_and_original_ids() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let parent = addr("session", "root");
        let child = addr("child-run", "worker");
        let first = router.register(parent.clone(), None).await.unwrap();
        router
            .record_parent_delivery_alias("parent-turn", &parent, &parent.agent_id)
            .await;
        router
            .record_sub_run(SubRunInfo {
                run_id: child.run_id.clone(),
                parent_run_id: "parent-turn".into(),
                agent_id: child.agent_id.clone(),
                depth: 1,
                delegation_id: "test".into(),
            })
            .await;
        let early = AgentMessage::new(
            child.clone(),
            MessageTarget::Parent,
            MessagePayload::Text {
                content: "early".into(),
                summary: None,
            },
        );
        let early_id = early.id.clone();
        router.send(early).await.unwrap();
        first.release_unconsumed().await.unwrap();

        let idle = AgentMessage::new(
            child.clone(),
            MessageTarget::Parent,
            MessagePayload::Text {
                content: "idle".into(),
                summary: None,
            },
        );
        let idle_id = idle.id.clone();
        router.send(idle).await.unwrap();
        let mut next = router.register(parent, None).await.unwrap();
        let fresh = AgentMessage::new(
            child,
            MessageTarget::Parent,
            MessagePayload::Text {
                content: "fresh".into(),
                summary: None,
            },
        );
        let fresh_id = fresh.id.clone();
        router.send(fresh).await.unwrap();
        for id in [early_id, idle_id, fresh_id] {
            assert_eq!(next.try_recv().unwrap().id, id);
        }
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
    async fn release_before_replacement_retains_original_message() {
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

        // A second owner cannot attach while the old stream is live.
        assert!(router.register(root.clone(), None).await.is_err());
        old.release_unconsumed().await.unwrap();
        let mut next = router.register(root, None).await.unwrap();
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
    async fn deferred_delivery_does_not_starve_later_messages() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let sender = addr("sender-run", "sender");
        let receiver = addr("receiver-run", "receiver");
        let mut mailbox = router.register(receiver.clone(), None).await.unwrap();
        router.register(sender.clone(), None).await.unwrap();
        for content in ["blocked", "later"] {
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

        let lease = mailbox.lease_bounded(1);
        assert!(matches!(
            &lease.messages()[0].payload,
            MessagePayload::Text { content, .. } if content == "blocked"
        ));
        lease.defer(false);

        let later = mailbox
            .try_recv()
            .expect("later delivery must remain ready");
        assert!(matches!(
            &later.payload,
            MessagePayload::Text { content, .. } if content == "later"
        ));
        assert!(mailbox.try_recv().is_none());
        mailbox.retry_deferred();
        assert!(mailbox.try_recv().is_none());
    }

    #[tokio::test]
    async fn deferred_delivery_retries_only_at_the_explicit_boundary() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let sender = addr("sender-run", "sender");
        let receiver = addr("receiver-run", "receiver");
        let mut mailbox = router.register(receiver.clone(), None).await.unwrap();
        router.register(sender.clone(), None).await.unwrap();
        router
            .send(AgentMessage::new(
                sender,
                MessageTarget::Direct { address: receiver },
                MessagePayload::Text {
                    content: "deferred".into(),
                    summary: None,
                },
            ))
            .await
            .unwrap();

        mailbox.lease_bounded(1).defer(true);
        assert!(mailbox.try_recv().is_none());
        mailbox.retry_deferred();
        assert!(mailbox.try_recv().is_some());
    }

    #[tokio::test]
    async fn same_id_redelivery_replaces_the_pending_ack_envelope() {
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let acknowledged = Arc::new(std::sync::Mutex::new(Vec::new()));
        let first = Arc::new(AgentMessage::new(
            addr("run-sender", "sender"),
            MessageTarget::Direct {
                address: addr("run-receiver", "receiver"),
            },
            MessagePayload::Text {
                content: "first delivery".into(),
                summary: None,
            },
        ));
        let mut current_value = (*first).clone();
        current_value.payload = MessagePayload::Text {
            content: "current delivery".into(),
            summary: None,
        };
        let current = Arc::new(current_value);
        let mut mailbox = AgentMailbox {
            subscription: MailboxSubscription::new(addr("run-receiver", "receiver")),
            lifetime: MailboxLifetime::new(addr("run-receiver", "receiver")),
            address: addr("run-receiver", "receiver"),
            stream: tokio::sync::Mutex::new(Box::new(AckRecordingStream { acknowledged })),
            buffered: tokio::sync::Mutex::new(VecDeque::new()),
            parked: VecDeque::new(),
            pending_acks: Vec::new(),
            router,
            terminal_cleanup_started: false,
        };

        mailbox.defer_acknowledgement(first);
        mailbox.defer_acknowledgement(current.clone());

        let pending = mailbox.take_adopted_acknowledgements();
        assert_eq!(pending.len(), 1);
        assert!(Arc::ptr_eq(&pending[0], &current));
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
            lifetime: MailboxLifetime::new(addr("receiver", "receiver")),
            address: addr("receiver", "receiver"),
            stream: tokio::sync::Mutex::new(Box::new(BlockingAckStream {
                entered: entered.clone(),
                release,
            })),
            buffered: tokio::sync::Mutex::new(VecDeque::from([message.clone()])),
            parked: VecDeque::new(),
            pending_acks: Vec::new(),
            router,
            terminal_cleanup_started: false,
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
            lifetime: MailboxLifetime::new(addr("run-review", "reviewer")),
            address: addr("run-review", "reviewer"),
            stream: tokio::sync::Mutex::new(Box::new(AckRecordingStream {
                acknowledged: Arc::clone(&acknowledged),
            })),
            buffered: tokio::sync::Mutex::new(VecDeque::new()),
            parked: VecDeque::new(),
            pending_acks: Vec::new(),
            router,
            terminal_cleanup_started: false,
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
    async fn partial_ack_failure_retries_only_the_unconfirmed_delivery() {
        let acknowledged = Arc::new(std::sync::Mutex::new(Vec::new()));
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            tracker(),
        ));
        let first = Arc::new(AgentMessage::new(
            addr("run-a", "sender"),
            MessageTarget::Direct {
                address: addr("run-b", "receiver"),
            },
            MessagePayload::Text {
                content: "first".into(),
                summary: None,
            },
        ));
        let second = Arc::new(AgentMessage::new(
            addr("run-a", "sender"),
            MessageTarget::Direct {
                address: addr("run-b", "receiver"),
            },
            MessagePayload::Text {
                content: "second".into(),
                summary: None,
            },
        ));
        let mailbox = AgentMailbox {
            subscription: MailboxSubscription::new(addr("run-b", "receiver")),
            lifetime: MailboxLifetime::new(addr("run-b", "receiver")),
            address: addr("run-b", "receiver"),
            stream: tokio::sync::Mutex::new(Box::new(SelectiveAckStream {
                failing_id: second.id.clone(),
                acknowledged: Arc::clone(&acknowledged),
            })),
            buffered: tokio::sync::Mutex::new(VecDeque::new()),
            parked: VecDeque::new(),
            pending_acks: Vec::new(),
            router,
            terminal_cleanup_started: false,
        };

        let (retry, error) = mailbox
            .acknowledge_received_with_failures(&[first.clone(), second.clone()])
            .await;
        assert!(matches!(error, Some(MailboxError::Transport(_))));
        assert_eq!(
            retry.iter().map(|message| &message.id).collect::<Vec<_>>(),
            [&second.id]
        );
        assert_eq!(
            acknowledged.lock().expect("ack recorder lock").as_slice(),
            [first.id.as_str()]
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
        let next = router.register(address, None).await.unwrap();
        assert_ne!(old.subscription(), next.subscription());
    }
}
