//! Request / outcome POD for [`crate::session_memory::MemoryExtractionService`].

use serde_json::Value;

use astra_turn_core::cloud_session_memory_extract::SessionMemoryExtractConfig;

/// Inputs for one extraction attempt. Owned so the whole bundle can
/// cross a `tokio::spawn` boundary without borrowing from turn state.
#[derive(Debug, Clone)]
pub struct ExtractionRequest {
    pub session_id: String,
    pub messages: Vec<Value>,
    pub current_tokens: usize,
    pub current_tool_calls: usize,
    pub had_error: bool,
    pub turn_number: u32,
    pub config: SessionMemoryExtractConfig,
}

/// Synchronous result returned to the caller immediately after
/// [`crate::session_memory::MemoryExtractionService::maybe_spawn`] decides
/// whether to spawn the background worker. Callers use this to update
/// debounce state.
#[derive(Debug, PartialEq, Eq)]
pub enum SpawnDecision {
    /// A background task was spawned.
    Spawned,
    /// Gate rejected the attempt (no session id, below init gate,
    /// debounced, or an extraction is already in flight). The
    /// corresponding `SessionMemoryExtraction{outcome="skipped"}` event
    /// has already been emitted synchronously.
    Skipped,
}
