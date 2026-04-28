//! Agent messaging framework for inter-agent communication.
//!
//! Re-exports from the `astra-messaging` crate, plus integration tests
//! that depend on runtime types (DelegationTracker, PermissionSync, etc.).


#[cfg(test)]
mod db_transport_integration_tests;
#[cfg(test)]
mod delegation_mailbox_tests;
#[cfg(test)]
mod e2e_loop_tests;
#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod orchestrator_mailbox_tests;

// Re-export key types for convenience.
pub use astra_messaging::{
    AckConfig, AckOutcome, AgentAddress, AgentMailbox, AgentMailboxRouter, AgentMessage,
    AgentSignal, CleanupScheduler, DatabaseTransport, DbTransportMetrics, DeadLetter,
    DeadLetterQueue, DeadLetterReason, DeadLetterSummary, DelegationLookup, EventDispatcher,
    InProcessMetrics, InProcessTransport, LatencySnapshot, LatencyTracker, MailboxError,
    MessagePayload, MessageStream, MessageTarget, MessageTransport, MessagingEvent,
    MessagingEventHandler, MessagingMetrics, MetricsSnapshot, PendingAckTracker, PermissionOutcome,
    RequestType, SendResult, StderrEventHandler, SubRunInfo,
};
pub use astra_messaging::{metrics, db_transport, transport, in_process, router, types};
