//! Agent messaging framework for inter-agent communication.
//!
//! Provides transport-agnostic message routing between agents in a delegation
//! hierarchy. Supports in-process (tokio channels) and database-backed
//! transports.

pub mod db_transport;
pub mod delegation;
pub mod in_process;
pub mod router;
pub mod transport;
pub mod types;

// Re-export key types for convenience.
pub use astra_turn_types::{
    AGENT_COMMUNICATION_SCHEMA_VERSION, AgentCommunicationDirection, AgentCommunicationEvent,
    AgentCommunicationParty, AgentCommunicationTarget,
};
pub use db_transport::{CleanupScheduler, DatabaseTransport};
pub use delegation::{DelegationLookup, SubRunInfo};
pub use in_process::InProcessTransport;
pub use router::{AgentMailbox, AgentMailboxRouter, MailboxCapabilities, PermissionOutcome};
pub use transport::{MessageStream, MessageTransport};
pub use types::{
    AgentAddress, AgentMessage, AgentSignal, DurableUserIntentReference, MailboxError,
    MessagePayload, MessageTarget, RequestType, agent_communication_event,
};
