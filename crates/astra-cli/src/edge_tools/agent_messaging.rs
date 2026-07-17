//! CLI mailbox binding for the canonical runtime `agent.send_message` path.
//!
//! Parsing, target resolution, payload mapping, delivery receipts, and error
//! semantics live in `astra_runtime::orchestration`. This module deliberately
//! retains only the execution context assembled by CLI hosts, preventing a
//! second public messaging protocol from drifting again.

use std::sync::Arc;

use astra_messaging::router::AgentMailboxRouter;

#[derive(Clone)]
pub struct SendMessageRuntimeContext {
    /// Current agent identity used as the message sender.
    pub agent_id: String,
    /// Canonical run that owns the current mailbox.
    pub run_id: String,
    /// Shared router used by the runtime-owned message handler.
    pub router: Arc<AgentMailboxRouter>,
}
