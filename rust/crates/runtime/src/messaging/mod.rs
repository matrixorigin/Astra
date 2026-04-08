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
//!   InProcessTransport     DatabaseTransport (future)
//!   (tokio channels,       (MySQL event log,
//!    µs latency)            ~10ms, zero new deps)
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

pub mod in_process;
pub mod router;
pub mod send_tool;
pub mod transport;
pub mod types;

// Re-export key types for convenience.
pub use in_process::InProcessTransport;
pub use router::{AgentMailbox, AgentMailboxRouter};
pub use transport::{MessageStream, MessageTransport};
pub use types::{
    AgentAddress, AgentMessage, AgentSignal, MailboxError, MessagePayload, MessageTarget,
    RequestType,
};
