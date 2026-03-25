/// Approximate token count using chars/4 heuristic (works for mixed EN/CN).
/// Adds overhead per message for role/formatting tokens, plus estimated
/// system prompt and tool schema overhead that the LLM API counts but
/// we don't see in the messages array.
pub fn estimate_tokens(messages: &[serde_json::Value]) -> usize {
    const PER_MESSAGE_OVERHEAD: usize = 4; // role + separators
    // System prompt (~1.2K tokens) + tool schemas (~1.5K tokens avg) + model framing
    const FIXED_OVERHEAD: usize = 3000;
    let message_tokens: usize = messages
        .iter()
        .map(|m| {
            let content_len = m
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            // Also count tool_calls content if present
            let tool_call_len = m
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|tc| {
                            tc.get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(|a| a.as_str())
                                .map(|s| s.len())
                                .unwrap_or(0)
                        })
                        .sum::<usize>()
                })
                .unwrap_or(0);
            (content_len + tool_call_len) / 4 + PER_MESSAGE_OVERHEAD
        })
        .sum();
    message_tokens + FIXED_OVERHEAD
}

/// Context budget configuration — model-aware limits.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ContextBudget {
    /// Maximum context tokens the model supports.
    pub model_limit: usize,
    /// Auto-compact fires when estimated tokens exceed this fraction of model_limit.
    pub compact_threshold: f64,
    /// Number of recent turns to keep after compaction.
    pub keep_recent_turns: usize,
    /// Max chars for memory retrieval injection.
    pub memory_budget_chars: usize,
}

impl ContextBudget {
    /// Returns the token count at which auto-compact should trigger.
    pub fn compact_trigger(&self) -> usize {
        (self.model_limit as f64 * self.compact_threshold) as usize
    }

    /// Whether the given token count exceeds the compact trigger.
    pub fn should_compact(&self, estimated_tokens: usize) -> bool {
        estimated_tokens > self.compact_trigger()
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            model_limit: 128_000,
            compact_threshold: 0.75,
            keep_recent_turns: 4,
            memory_budget_chars: 8_000,
        }
    }
}

/// Return a ContextBudget tuned for a known model name.
pub fn budget_for_model(model: Option<&str>) -> ContextBudget {
    let limit = match model.unwrap_or("") {
        m if m.contains("gpt-4o") || m.contains("gpt-4.1") => 128_000,
        m if m.contains("gpt-4-turbo") => 128_000,
        m if m.contains("gpt-3.5") => 16_000,
        m if m.contains("claude") => 200_000,
        m if m.contains("deepseek") => 64_000,
        m if m.contains("kimi") || m.contains("moonshot") => 128_000,
        m if m.contains("qwen") => 128_000,
        _ => 128_000, // safe default
    };
    ContextBudget {
        model_limit: limit,
        ..Default::default()
    }
}
