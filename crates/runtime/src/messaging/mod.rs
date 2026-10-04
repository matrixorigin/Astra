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
pub mod reply_obligations;

// Re-export key types for convenience.
pub use astra_messaging::{
    AgentAddress, AgentMailbox, AgentMailboxRouter, AgentMessage, AgentSignal, CleanupScheduler,
    DatabaseTransport, DelegationLookup, InProcessTransport, MailboxError, MessagePayload,
    MessageStream, MessageTarget, MessageTransport, PermissionOutcome, RequestType, SubRunInfo,
};
pub use astra_messaging::{db_transport, in_process, router, transport, types};
