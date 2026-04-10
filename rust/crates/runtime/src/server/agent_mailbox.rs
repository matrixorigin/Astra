//! Bidirectional Agent Communication (D-8)
//!
//! An in-memory mailbox system for agent-to-agent messaging, inspired by
//! Claude Code's SendMessageTool + file-system mailbox architecture.
//!
//! Key features:
//! - Unicast (agent-to-agent) and broadcast messaging
//! - Structured message types (text, shutdown, status updates)
//! - Async notification via `tokio::sync::Notify` (no polling)
//! - Thread-safe via `RwLock<HashMap>`

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};
use tokio::sync::Notify;

// ───────────────────────────── Message Types ─────────────────────────────

/// Type of agent message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageType {
    /// Free-form text message.
    Text,
    /// Request to shut down gracefully.
    ShutdownRequest { request_id: String },
    /// Acknowledgement of shutdown request.
    ShutdownApproved { request_id: String },
    /// Progress/status update from a running agent.
    StatusUpdate { progress_pct: u8, detail: String },
}

/// A message sent between agents.
#[derive(Debug, Clone)]
pub struct AgentMessage {
    /// Sender agent ID.
    pub from: String,
    /// Recipient agent ID, or `"*"` for broadcast.
    pub to: String,
    /// Message content.
    pub content: String,
    /// Structured message type.
    pub msg_type: MessageType,
    /// When the message was created.
    pub timestamp: SystemTime,
}

impl AgentMessage {
    pub fn text(from: &str, to: &str, content: &str) -> Self {
        Self {
            from: from.to_string(),
            to: to.to_string(),
            content: content.to_string(),
            msg_type: MessageType::Text,
            timestamp: SystemTime::now(),
        }
    }

    pub fn shutdown_request(from: &str, to: &str, request_id: &str) -> Self {
        Self {
            from: from.to_string(),
            to: to.to_string(),
            content: format!("Shutdown requested: {}", request_id),
            msg_type: MessageType::ShutdownRequest {
                request_id: request_id.to_string(),
            },
            timestamp: SystemTime::now(),
        }
    }

    pub fn shutdown_approved(from: &str, to: &str, request_id: &str) -> Self {
        Self {
            from: from.to_string(),
            to: to.to_string(),
            content: format!("Shutdown approved: {}", request_id),
            msg_type: MessageType::ShutdownApproved {
                request_id: request_id.to_string(),
            },
            timestamp: SystemTime::now(),
        }
    }

    pub fn status_update(from: &str, to: &str, progress_pct: u8, detail: &str) -> Self {
        Self {
            from: from.to_string(),
            to: to.to_string(),
            content: detail.to_string(),
            msg_type: MessageType::StatusUpdate {
                progress_pct,
                detail: detail.to_string(),
            },
            timestamp: SystemTime::now(),
        }
    }

    pub fn is_broadcast(&self) -> bool {
        self.to == "*"
    }
}

// ───────────────────────────── Mailbox ────────────────────────────────────

/// Internal state behind a single RwLock to prevent deadlocks from
/// multi-lock acquisition ordering.
struct MailboxState {
    queues: HashMap<String, VecDeque<AgentMessage>>,
    notifiers: HashMap<String, Arc<Notify>>,
    agents: Vec<String>,
}

/// Thread-safe in-memory mailbox for agent communication.
///
/// Each registered agent has a queue of incoming messages and a `Notify` handle
/// for efficient async waiting.
#[derive(Clone)]
pub struct AgentMailbox {
    state: Arc<RwLock<MailboxState>>,
}

impl AgentMailbox {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(MailboxState {
                queues: HashMap::new(),
                notifiers: HashMap::new(),
                agents: Vec::new(),
            })),
        }
    }

    /// Register an agent so it can receive messages.
    pub fn register(&self, agent_id: &str) {
        let id = agent_id.to_string();
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        s.queues.entry(id.clone()).or_default();
        s.notifiers
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Notify::new()));
        if !s.agents.contains(&id) {
            s.agents.push(id);
        }
    }

    /// Unregister an agent, dropping its queue.
    pub fn unregister(&self, agent_id: &str) {
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        s.queues.remove(agent_id);
        s.notifiers.remove(agent_id);
        s.agents.retain(|a| a != agent_id);
    }

    /// Send a message. If `to` is `"*"`, broadcasts to all registered agents
    /// except the sender.
    pub fn send(&self, msg: AgentMessage) {
        if msg.is_broadcast() {
            let from = msg.from.clone();
            let s = self.state.read().unwrap_or_else(|e| e.into_inner());
            let targets: Vec<String> = s.agents.iter().filter(|a| **a != from).cloned().collect();
            drop(s); // release read lock before enqueue which needs write
            for target in targets {
                let mut cloned = msg.clone();
                cloned.to = target.clone();
                self.enqueue(&target, cloned);
            }
        } else {
            let target = msg.to.clone();
            self.enqueue(&target, msg);
        }
    }

    fn enqueue(&self, agent_id: &str, msg: AgentMessage) {
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        if let Some(q) = s.queues.get_mut(agent_id) {
            q.push_back(msg);
        }
        let notifier = s.notifiers.get(agent_id).cloned();
        drop(s); // release lock before notify
        if let Some(n) = notifier {
            n.notify_one();
        }
    }

    /// Try to receive a message without waiting. Returns `None` if queue is empty.
    pub fn try_recv(&self, agent_id: &str) -> Option<AgentMessage> {
        self.state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .queues
            .get_mut(agent_id)?
            .pop_front()
    }

    /// Drain all pending messages for an agent.
    pub fn drain(&self, agent_id: &str) -> Vec<AgentMessage> {
        self.state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .queues
            .get_mut(agent_id)
            .map(|q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Wait for a message with timeout. Returns `None` on timeout.
    pub async fn recv(&self, agent_id: &str, timeout: Duration) -> Option<AgentMessage> {
        if let Some(msg) = self.try_recv(agent_id) {
            return Some(msg);
        }

        let notifier = self.state.read().unwrap_or_else(|e| e.into_inner()).notifiers.get(agent_id)?.clone();

        tokio::select! {
            _ = notifier.notified() => {
                self.try_recv(agent_id)
            }
            _ = tokio::time::sleep(timeout) => None,
        }
    }

    /// Number of pending messages for an agent.
    pub fn pending_count(&self, agent_id: &str) -> usize {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .queues
            .get(agent_id)
            .map_or(0, |q| q.len())
    }

    /// List all registered agent IDs.
    pub fn registered_agents(&self) -> Vec<String> {
        self.state.read().unwrap_or_else(|e| e.into_inner()).agents.clone()
    }
}

impl Default for AgentMailbox {
    fn default() -> Self {
        Self::new()
    }
}

// ───────────────────────────── Tests ─────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_send_unicast() {
        let mb = AgentMailbox::new();
        mb.register("alice");
        mb.register("bob");

        mb.send(AgentMessage::text("alice", "bob", "hello bob"));
        assert_eq!(mb.pending_count("bob"), 1);
        assert_eq!(mb.pending_count("alice"), 0);

        let msg = mb.try_recv("bob").unwrap();
        assert_eq!(msg.from, "alice");
        assert_eq!(msg.content, "hello bob");
        assert_eq!(mb.pending_count("bob"), 0);
    }

    #[test]
    fn broadcast_reaches_all_except_sender() {
        let mb = AgentMailbox::new();
        mb.register("lead");
        mb.register("alice");
        mb.register("bob");

        mb.send(AgentMessage::text("lead", "*", "attention everyone"));

        assert_eq!(mb.pending_count("lead"), 0); // sender excluded
        assert_eq!(mb.pending_count("alice"), 1);
        assert_eq!(mb.pending_count("bob"), 1);

        let msg = mb.try_recv("alice").unwrap();
        assert_eq!(msg.from, "lead");
        assert_eq!(msg.content, "attention everyone");
    }

    #[test]
    fn drain_collects_all() {
        let mb = AgentMailbox::new();
        mb.register("alice");

        mb.send(AgentMessage::text("bob", "alice", "msg1"));
        mb.send(AgentMessage::text("charlie", "alice", "msg2"));
        mb.send(AgentMessage::text("dave", "alice", "msg3"));

        let msgs = mb.drain("alice");
        assert_eq!(msgs.len(), 3);
        assert_eq!(mb.pending_count("alice"), 0);
    }

    #[test]
    fn unregister_drops_queue() {
        let mb = AgentMailbox::new();
        mb.register("alice");
        mb.send(AgentMessage::text("bob", "alice", "hello"));
        assert_eq!(mb.pending_count("alice"), 1);

        mb.unregister("alice");
        assert_eq!(mb.pending_count("alice"), 0);
        assert!(mb.try_recv("alice").is_none());
    }

    #[test]
    fn shutdown_request_response_protocol() {
        let mb = AgentMailbox::new();
        mb.register("lead");
        mb.register("worker");

        // Lead sends shutdown request
        mb.send(AgentMessage::shutdown_request("lead", "worker", "req-001"));
        let msg = mb.try_recv("worker").unwrap();
        match &msg.msg_type {
            MessageType::ShutdownRequest { request_id } => {
                assert_eq!(request_id, "req-001");
            }
            _ => panic!("expected ShutdownRequest"),
        }

        // Worker approves
        mb.send(AgentMessage::shutdown_approved("worker", "lead", "req-001"));
        let resp = mb.try_recv("lead").unwrap();
        match &resp.msg_type {
            MessageType::ShutdownApproved { request_id } => {
                assert_eq!(request_id, "req-001");
            }
            _ => panic!("expected ShutdownApproved"),
        }
    }

    #[test]
    fn status_update_message() {
        let mb = AgentMailbox::new();
        mb.register("lead");
        mb.register("worker");

        mb.send(AgentMessage::status_update(
            "worker",
            "lead",
            75,
            "3/4 files processed",
        ));
        let msg = mb.try_recv("lead").unwrap();
        match &msg.msg_type {
            MessageType::StatusUpdate {
                progress_pct,
                detail,
            } => {
                assert_eq!(*progress_pct, 75);
                assert_eq!(detail, "3/4 files processed");
            }
            _ => panic!("expected StatusUpdate"),
        }
    }

    #[test]
    fn send_to_unregistered_agent_is_silent() {
        let mb = AgentMailbox::new();
        mb.register("alice");
        // Sending to unregistered "bob" should not panic
        mb.send(AgentMessage::text("alice", "bob", "hello?"));
        assert_eq!(mb.pending_count("bob"), 0);
    }

    #[test]
    fn registered_agents_list() {
        let mb = AgentMailbox::new();
        mb.register("alice");
        mb.register("bob");
        mb.register("charlie");

        let mut agents = mb.registered_agents();
        agents.sort();
        assert_eq!(agents, vec!["alice", "bob", "charlie"]);
    }

    #[tokio::test]
    async fn async_recv_immediate() {
        let mb = AgentMailbox::new();
        mb.register("alice");
        mb.send(AgentMessage::text("bob", "alice", "instant"));

        let msg = mb.recv("alice", Duration::from_millis(100)).await;
        assert!(msg.is_some());
        assert_eq!(msg.unwrap().content, "instant");
    }

    #[tokio::test]
    async fn async_recv_timeout() {
        let mb = AgentMailbox::new();
        mb.register("alice");

        let msg = mb.recv("alice", Duration::from_millis(50)).await;
        assert!(msg.is_none());
    }

    #[tokio::test]
    async fn async_recv_with_delayed_send() {
        let mb = AgentMailbox::new();
        mb.register("alice");

        let mb2 = mb.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            mb2.send(AgentMessage::text("bob", "alice", "delayed"));
        });

        let msg = mb.recv("alice", Duration::from_secs(1)).await;
        assert!(msg.is_some());
        assert_eq!(msg.unwrap().content, "delayed");
    }
}
