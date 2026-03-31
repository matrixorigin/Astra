//! Session Memory extraction service.
//!
//! Provides async extraction trigger logic that can be integrated
//! with the agentic loop.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use super::session_memory::{SessionMemory, SessionMemoryConfig, SessionMemoryState};
use super::sm_extract::build_extraction_prompt;
use super::summary::SummaryLlmClient;

/// Service for managing session memory extraction.
///
/// This is designed to be shared across turns and manages the
/// async extraction lifecycle.
pub struct SessionMemoryService {
    /// The session memory instance (file + content).
    pub memory: SessionMemory,
    /// Extraction state (initialized, tokens, etc.).
    pub state: SessionMemoryState,
    /// Configuration.
    pub config: SessionMemoryConfig,
    /// Lock to prevent concurrent extractions.
    extraction_lock: Mutex<()>,
}

impl SessionMemoryService {
    /// Create a new session memory service.
    pub fn new(session_dir: impl Into<std::path::PathBuf>, config: SessionMemoryConfig) -> Self {
        let mut memory = SessionMemory::new(session_dir, config.clone());

        // Try to load existing memory, or initialize with template
        if memory.load().is_err() {
            memory.init_with_template();
        }

        Self {
            memory,
            state: SessionMemoryState::default(),
            config,
            extraction_lock: Mutex::new(()),
        }
    }

    /// Check if extraction should be triggered.
    pub fn should_extract(&self, current_tokens: usize, has_pending_tool_calls: bool) -> bool {
        self.state
            .should_extract(&self.config, current_tokens, has_pending_tool_calls)
    }

    /// Record a tool call for extraction trigger tracking.
    pub fn record_tool_call(&mut self) {
        self.state.record_tool_call();
    }

    /// Attempt to trigger extraction if conditions are met.
    ///
    /// Returns `Some(handle)` if extraction was started, `None` if skipped.
    pub fn try_trigger_extraction(
        self: &Arc<Self>,
        messages: Vec<Value>,
        current_tokens: usize,
        current_turn: u32,
        last_message_id: Option<String>,
        llm_client: Arc<dyn SummaryLlmClient>,
    ) -> Option<tokio::task::JoinHandle<Result<(), String>>> {
        // Check if we should extract
        if !self.should_extract(current_tokens, false) {
            return None;
        }

        // Check if extraction is already in progress
        if self.state.extraction_in_progress {
            // Check for timeout
            if self.state.is_extraction_timed_out(15) {
                eprintln!("[sm_service] Extraction timed out, will retry next turn");
            }
            return None;
        }

        // Clone self for the async task
        let service = Arc::clone(self);

        Some(tokio::spawn(async move {
            service
                .run_extraction(
                    messages,
                    current_tokens,
                    current_turn,
                    last_message_id,
                    llm_client,
                )
                .await
        }))
    }

    /// Run the extraction process.
    async fn run_extraction(
        self: &Arc<Self>,
        messages: Vec<Value>,
        current_tokens: usize,
        current_turn: u32,
        _last_message_id: Option<String>,
        llm_client: Arc<dyn SummaryLlmClient>,
    ) -> Result<(), String> {
        // Acquire lock to prevent concurrent extractions
        let _guard = self.extraction_lock.lock().await;

        // Mark extraction started (need interior mutability here in real impl)
        eprintln!(
            "[sm_service] Starting extraction at turn {}, tokens {}",
            current_turn, current_tokens
        );

        // Find messages since last extraction
        let messages_since = self.get_messages_since_last_extraction(&messages);

        if messages_since.is_empty() {
            eprintln!("[sm_service] No new messages since last extraction, skipping");
            return Ok(());
        }

        // Build extraction prompt
        let prompt = build_extraction_prompt(&self.memory, &messages_since);

        // Build a single-message payload for the LLM
        let extraction_messages = vec![serde_json::json!({
            "role": "user",
            "content": prompt
        })];

        // Call LLM via the SummaryLlmClient trait
        let response = llm_client
            .summarize(&extraction_messages)
            .await
            .map_err(|e| format!("LLM extraction failed: {e}"))?;

        if response.is_ptl_error {
            return Err("Extraction prompt too long".to_string());
        }

        // Parse and update memory (would need RefCell or similar for real mutation)
        // For now, just log success
        eprintln!(
            "[sm_service] Extraction completed, response {} chars",
            response.text.len()
        );

        // In real implementation:
        // - parse_extraction_response(&mut self.memory, &response.text)?;
        // - self.memory.save()?;
        // - self.state.complete_extraction(current_turn, last_message_id, current_tokens);

        Ok(())
    }

    /// Get messages since the last extraction boundary.
    fn get_messages_since_last_extraction(&self, messages: &[Value]) -> Vec<Value> {
        if let Some(ref boundary_id) = self.state.last_summarized_message_id {
            // Find the boundary message
            let boundary_idx = messages.iter().position(|m| {
                m.get("id")
                    .and_then(Value::as_str)
                    .map(|id| id == boundary_id)
                    .unwrap_or(false)
            });

            if let Some(idx) = boundary_idx {
                return messages[idx + 1..].to_vec();
            }
        }

        // No boundary or not found - return all messages
        messages.to_vec()
    }
}

// ---------------------------------------------------------------------------
// Integration helpers
// ---------------------------------------------------------------------------

/// Estimate total tokens from messages.
pub fn estimate_messages_tokens(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(|m| {
            let content = m.get("content").and_then(Value::as_str).unwrap_or("");
            crate::prompts::estimate_str_tokens(content) + 4 // +4 for role overhead
        })
        .sum()
}

/// Extract the ID of the last message (if present).
pub fn get_last_message_id(messages: &[Value]) -> Option<String> {
    messages
        .last()
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
        .map(String::from)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn service_should_extract_logic() {
        let service = SessionMemoryService::new("/tmp/test", SessionMemoryConfig::default());

        // Not initialized, should not extract
        assert!(!service.should_extract(5000, false));

        // Would need to modify state for more tests
    }

    #[test]
    fn get_messages_since_last_extraction_no_boundary() {
        let service = SessionMemoryService::new("/tmp/test", SessionMemoryConfig::default());

        let messages = vec![
            json!({"role": "user", "content": "q1", "id": "m1"}),
            json!({"role": "assistant", "content": "a1", "id": "m2"}),
        ];

        let since = service.get_messages_since_last_extraction(&messages);
        assert_eq!(since.len(), 2);
    }

    #[test]
    fn estimate_messages_tokens_basic() {
        let messages = vec![
            json!({"role": "user", "content": "hello world"}),
            json!({"role": "assistant", "content": "hi there"}),
        ];

        let tokens = estimate_messages_tokens(&messages);
        assert!(tokens > 0);
        assert!(tokens < 50); // Should be small for short messages
    }

    #[test]
    fn get_last_message_id_found() {
        let messages = vec![
            json!({"role": "user", "content": "q", "id": "msg-1"}),
            json!({"role": "assistant", "content": "a", "id": "msg-2"}),
        ];

        assert_eq!(get_last_message_id(&messages), Some("msg-2".to_string()));
    }

    #[test]
    fn get_last_message_id_missing() {
        let messages = vec![json!({"role": "user", "content": "q"})];

        assert_eq!(get_last_message_id(&messages), None);
    }
}
