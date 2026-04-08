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

pub mod db_transport;
pub mod in_process;
pub mod router;
pub mod send_tool;
pub mod transport;
pub mod types;

#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod e2e_loop_tests;
#[cfg(test)]
mod db_transport_integration_tests;
#[cfg(test)]
mod delegation_mailbox_tests;

// Re-export key types for convenience.
pub use db_transport::{CleanupScheduler, DatabaseTransport, TransportMetrics as DbTransportMetrics};
pub use in_process::{InProcessMetrics, InProcessTransport};
pub use router::{AgentMailbox, AgentMailboxRouter};
pub use transport::{MessageStream, MessageTransport};
pub use types::{
    AgentAddress, AgentMessage, AgentSignal, MailboxError, MessagePayload, MessageTarget,
    RequestType,
};
