//! Request / outcome POD for [`crate::session_memory::MemoryExtractionService`].

use serde_json::Value;

use astra_turn_types::session_facts::SessionFacts;

/// Inputs for one extraction attempt. Owned so the whole bundle can
/// cross a `tokio::spawn` boundary without borrowing from turn state.
#[derive(Debug, Clone)]
pub struct ExtractionRequest {
    pub inference_scope: astra_turn_types::InferenceInvocationScope,
    pub messages: Vec<Value>,
    pub session_facts: SessionFacts,
    pub had_error: bool,
    pub reanchors_current_objective: bool,
}

impl ExtractionRequest {
    #[must_use]
    pub fn session_id(&self) -> &str {
        self.inference_scope.session_id()
    }

    #[must_use]
    pub fn turn_number(&self) -> u32 {
        self.inference_scope.turn()
    }
}

/// Synchronous result returned to the caller immediately after
/// [`crate::session_memory::MemoryExtractionService::maybe_spawn`] decides
/// whether to spawn the background worker. Callers use this to update
/// debounce state.
#[derive(Debug, PartialEq, Eq)]
pub enum SpawnDecision {
    /// A background task was spawned.
    Spawned,
    /// A different semantic snapshot arrived while this session already had
    /// a worker. The service retained the latest request and the current
    /// worker will process it before releasing the session slot.
    Queued,
    /// Gate rejected the attempt (no owner/session id, below init gate,
    /// debounced, or an extraction is already in flight). Owner-bound gate
    /// rejections emit a synchronous `SessionMemoryExtraction` skipped event;
    /// an unbound process template has no legal owner for durable emission and
    /// instead fails loudly in the runtime log.
    Skipped,
}
