//! Agent messaging framework for inter-agent communication.
//!
//! # Architecture
//!
//! ```text
//! ┌───────────────────────────────────────────────────┐
//! │              AgentMailboxRouter                    │
//! │  (resolves Parent/Broadcast targets,              │
//! │   dispatches via pluggable transport)              │
//! └───────────────┬───────────────────────────────────┘
//!                 │
//!        ┌────────┴─────────┐
//!        │ MessageTransport │  ← trait
//!        └────────┬─────────┘
//!            ┌────┴─────────────────┐
//!            │                      │
//!   InProcessTransport     DatabaseTransport
//!   (tokio channels,       (MySQL poll-based,
//!    µs latency)            ~100ms, cross-process)
//! ```
//!
//! # Usage
//!
//! ```no_run
//! use astra_runtime::messaging::{
//!     AgentAddress, AgentMailboxRouter, InProcessTransport,
//! };
//!
//! # async fn example() {
//! let transport = std::sync::Arc::new(InProcessTransport::new());
//! // let tracker = ...; // DelegationTracker from DelegationEngine
//! // let router = std::sync::Arc::new(AgentMailboxRouter::new(transport, tracker));
//! // let mailbox = router.register(AgentAddress::new("run-1", "coder"), None).await.unwrap();
//! # }
//! ```

pub mod ack_tracker;
pub mod db_transport;
pub mod dead_letter;
pub mod in_process;
pub mod metrics;
pub mod router;
pub mod send_tool;
pub mod transport;
pub mod types;

#[cfg(test)]
mod db_transport_integration_tests;
#[cfg(test)]
mod delegation_mailbox_tests;
#[cfg(test)]
mod e2e_loop_tests;
#[cfg(test)]
mod integration_tests;

// Re-export key types for convenience.
pub use ack_tracker::{AckConfig, AckOutcome, PendingAckTracker};
pub use db_transport::{
    CleanupScheduler, DatabaseTransport, TransportMetrics as DbTransportMetrics,
};
pub use dead_letter::{DeadLetter, DeadLetterQueue, DeadLetterReason, DeadLetterSummary};
pub use in_process::{InProcessMetrics, InProcessTransport};
pub use metrics::{
    EventDispatcher, LatencySnapshot, LatencyTracker, MessagingEvent, MessagingEventHandler,
    MessagingMetrics, MetricsSnapshot, StderrEventHandler,
};
pub use router::{AgentMailbox, AgentMailboxRouter};
pub use send_tool::SendResult;
pub use transport::{MessageStream, MessageTransport};
pub use types::{
    AgentAddress, AgentMessage, AgentSignal, MailboxError, MessagePayload, MessageTarget,
    RequestType,
};
