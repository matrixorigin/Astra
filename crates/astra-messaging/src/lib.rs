//! Agent messaging framework for inter-agent communication.
//!
//! Provides transport-agnostic message routing between agents in a delegation
//! hierarchy. Supports in-process (tokio channels) and database-backed
//! transports.

pub mod ack_tracker;
pub mod db_transport;
pub mod dead_letter;
pub mod delegation;
pub mod in_process;
pub mod metrics;
pub mod router;
pub mod send_tool;
pub mod transport;
pub mod types;

// Re-export key types for convenience.
pub use ack_tracker::{AckConfig, AckOutcome, PendingAckTracker};
pub use db_transport::{
    CleanupScheduler, DatabaseTransport, TransportMetrics as DbTransportMetrics,
};
pub use dead_letter::{DeadLetter, DeadLetterQueue, DeadLetterReason, DeadLetterSummary};
pub use delegation::{DelegationLookup, SubRunInfo};
pub use in_process::{InProcessMetrics, InProcessTransport};
pub use metrics::{
    EventDispatcher, LatencySnapshot, LatencyTracker, MessagingEvent, MessagingEventHandler,
    MessagingMetrics, MetricsSnapshot, StderrEventHandler,
};
pub use router::{AgentMailbox, AgentMailboxRouter, PermissionOutcome};
pub use send_tool::SendResult;
pub use transport::{MessageStream, MessageTransport};
pub use types::{
    AgentAddress, AgentMessage, AgentSignal, MailboxError, MessagePayload, MessageTarget,
    RequestType,
};
