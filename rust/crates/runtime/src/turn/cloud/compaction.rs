use crate::prompts::{CompactConfig, CompactionTier};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Compaction Types
// ---------------------------------------------------------------------------

/// What triggered the compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    /// User requested manual compaction (e.g., /compact command).
    Manual,
    /// Automatic compaction triggered by token budget pressure.
    Auto,
}

/// Metadata about a compaction event, injected as a boundary marker.
///
/// This allows post-compaction context (LLM, debugging, analytics) to understand
/// what happened and what was preserved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactBoundary {
    /// What triggered the compaction.
    pub trigger: CompactTrigger,
    /// Compaction tier used (determines aggressiveness).
    pub tier: CompactionTier,
    /// Estimated tokens before compaction.
    pub pre_tokens: usize,
    /// Number of messages before compaction.
    pub messages_before: usize,
    /// Number of messages after compaction.
    pub messages_after: usize,
    /// UUID of the last message before compaction (for linking).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_pre_compact_uuid: Option<String>,
    /// LLM-generated summary (Phase 2 feature).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Files that were recently accessed and may be restored as attachments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_files: Vec<String>,
}

impl CompactBoundary {
    /// Create a new compaction boundary marker.
    pub fn new(trigger: CompactTrigger, tier: CompactionTier) -> Self {
        Self {
            trigger,
            tier,
            pre_tokens: 0,
            messages_before: 0,
            messages_after: 0,
            last_pre_compact_uuid: None,
            summary: None,
            recent_files: Vec::new(),
        }
    }

    /// Set pre-compaction metrics.
    pub fn with_pre_metrics(mut self, tokens: usize, message_count: usize) -> Self {
        self.pre_tokens = tokens;
        self.messages_before = message_count;
        self
    }

    /// Set post-compaction message count.
    pub fn with_post_count(mut self, message_count: usize) -> Self {
        self.messages_after = message_count;
        self
    }

    /// Set the last pre-compact message UUID for linking.
    pub fn with_last_uuid(mut self, uuid: impl Into<String>) -> Self {
        self.last_pre_compact_uuid = Some(uuid.into());
        self
    }

    /// Set recent files for potential attachment restoration.
    pub fn with_recent_files(mut self, files: Vec<String>) -> Self {
        self.recent_files = files;
        self
    }

    /// Convert to a system message for injection into the message stream.
    pub fn to_system_message(&self) -> Value {
        serde_json::json!({
            "role": "system",
            "content": self.format_content(),
            "compact_metadata": self,
        })
    }

    /// Format human-readable content for the boundary message.
    fn format_content(&self) -> String {
        let trigger_str = match self.trigger {
            CompactTrigger::Manual => "manually",
            CompactTrigger::Auto => "automatically",
        };
        let tier_str = match self.tier {
            CompactionTier::Normal => "normal",
            CompactionTier::TrimSchemas => "trim-schemas",
            CompactionTier::CompactHistory => "compact-history",
            CompactionTier::AggressivePrune => "aggressive-prune",
        };
        format!(
            "[Conversation compacted {} (tier: {}, {} → {} messages)]",
            trigger_str, tier_str, self.messages_before, self.messages_after
        )
    }
}

/// Result of a compaction operation.
#[derive(Debug, Clone)]
pub struct CompactResult {
    /// Compacted messages.
    pub messages: Vec<Value>,
    /// Compaction boundary metadata (None if no compaction occurred).
    pub boundary: Option<CompactBoundary>,
    /// The tier that was applied.
    pub tier: CompactionTier,
}

/// Tier-aware compaction: applies progressively more aggressive strategies.
///
/// * `Normal` — no compaction, return messages unchanged.
/// * `TrimSchemas` — compact only tool results longer than `keep_chars * 2`.
/// * `CompactHistory` — compact all tool results to `keep_chars` (original behavior).
/// * `AggressivePrune` — compact tool results to `keep_chars / 2`, then drop old
///   user/assistant pairs beyond `keep_recent_turns`.
///
/// Returns the compacted message list (backward-compatible). For rich metadata,
/// use [`compact_tiered_with_result`].
pub fn compact_tiered(
    messages: &[Value],
    budget_chars: usize,
    keep_chars: usize,
    tier: CompactionTier,
    keep_recent_turns: usize,
) -> Vec<Value> {
    compact_tiered_with_result(messages, budget_chars, keep_chars, tier, keep_recent_turns)
        .messages
}

/// Tier-aware compaction returning a [`CompactResult`] with rich metadata.
///
/// Same algorithm as [`compact_tiered`], but also returns a [`CompactBoundary`]
/// when compaction actually occurred (tier != Normal and over budget).
pub fn compact_tiered_with_result(
    messages: &[Value],
    budget_chars: usize,
    keep_chars: usize,
    tier: CompactionTier,
    keep_recent_turns: usize,
) -> CompactResult {
    let messages_before = messages.len();

    if tier == CompactionTier::Normal {
        return CompactResult {
            messages: messages.to_vec(),
            boundary: None,
            tier,
        };
    }

    let total_chars: usize = messages
        .iter()
        .map(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|s| s.chars().count())
                .unwrap_or(0)
        })
        .sum();

    if total_chars <= budget_chars {
        return CompactResult {
            messages: messages.to_vec(),
            boundary: None,
            tier,
        };
    }

    let mut compacted = messages.to_vec();

    // Determine truncation limit per tier
    let trunc_limit = match tier {
        CompactionTier::Normal => unreachable!(),
        CompactionTier::TrimSchemas => keep_chars * 2,
        CompactionTier::CompactHistory => keep_chars,
        CompactionTier::AggressivePrune => keep_chars / 2,
    };

    // Truncate tool results (skip the last one — may be in-flight)
    let tool_indices: Vec<usize> = compacted
        .iter()
        .enumerate()
        .filter_map(|(i, m)| (m.get("role").and_then(Value::as_str) == Some("tool")).then_some(i))
        .collect();
    let compact_limit = tool_indices.len().saturating_sub(1);
    for &index in tool_indices.iter().take(compact_limit) {
        let content = compacted[index]
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if content.chars().count() > trunc_limit {
            let truncated: String = content.chars().take(trunc_limit).collect();
            compacted[index]["content"] =
                Value::String(truncated + "\n...[compacted for context budget]");
        }
    }

    // CompactHistory+: truncate older assistant messages.
    // LLM responses can be very verbose (1000+ tokens each).  Keeping full text
    // from early turns wastes context when only recent answers matter.
    // We preserve the last `keep_recent_turns` assistant messages in full.
    if matches!(
        tier,
        CompactionTier::CompactHistory | CompactionTier::AggressivePrune
    ) {
        let assistant_indices: Vec<usize> = compacted
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                (m.get("role").and_then(Value::as_str) == Some("assistant")).then_some(i)
            })
            .collect();
        let asst_limit = trunc_limit * 2; // generous limit for assistant text
        if assistant_indices.len() > keep_recent_turns {
            let compact_count = assistant_indices.len() - keep_recent_turns;
            for &index in assistant_indices.iter().take(compact_count) {
                let content = compacted[index]
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if content.chars().count() > asst_limit {
                    let truncated: String = content.chars().take(asst_limit).collect();
                    compacted[index]["content"] =
                        Value::String(truncated + "\n...[earlier response compacted]");
                }
            }
        }
    }

    // AggressivePrune: also drop old conversation turns
    if tier == CompactionTier::AggressivePrune {
        // Count user/assistant message pairs (excluding system and tool messages)
        let conv_indices: Vec<usize> = compacted
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                let role = m.get("role").and_then(Value::as_str).unwrap_or("");
                (role == "user" || role == "assistant").then_some(i)
            })
            .collect();
        // Keep only the last `keep_recent_turns * 2` conversation messages
        let keep_count = keep_recent_turns * 2;
        if conv_indices.len() > keep_count {
            let drop_set: std::collections::HashSet<usize> = conv_indices
                [..conv_indices.len() - keep_count]
                .iter()
                .copied()
                .collect();
            compacted = compacted
                .into_iter()
                .enumerate()
                .filter(|(i, _)| !drop_set.contains(i))
                .map(|(_, m)| m)
                .collect();
        }
    }

    let messages_after = compacted.len();
    let boundary = CompactBoundary::new(CompactTrigger::Auto, tier)
        .with_pre_metrics(0, messages_before)
        .with_post_count(messages_after);

    CompactResult {
        messages: compacted,
        boundary: Some(boundary),
        tier,
    }
}

/// Async compaction that optionally generates an LLM summary.
///
/// When `compact_config.should_summarize(tier)` is true, this function
/// calls the LLM to generate a semantic summary and stores it in the
/// returned [`CompactBoundary`]. If summary generation fails or is disabled,
/// it falls back to pure truncation via [`compact_tiered_with_result`].
///
/// The summary (if produced) is prepended as a user message to the
/// compacted history so the LLM has access to conversation context.
pub async fn compact_with_summary(
    messages: &[Value],
    budget_chars: usize,
    keep_chars: usize,
    tier: CompactionTier,
    keep_recent_turns: usize,
    compact_config: &CompactConfig,
    llm_client: Option<&dyn super::summary::SummaryLlmClient>,
) -> CompactResult {
    // Always run structural compaction first
    let mut result = compact_tiered_with_result(messages, budget_chars, keep_chars, tier, keep_recent_turns);

    // Attempt LLM summary if configured and a client is provided
    if compact_config.should_summarize(tier) {
        if let Some(client) = llm_client {
            match super::summary::generate_compact_summary(messages, client).await {
                Some(summary) => {
                    // Prepend summary as a user message
                    let summary_msg = serde_json::json!({
                        "role": "user",
                        "content": format!("[Conversation summary — context compacted]\n\n{summary}"),
                        "attachment_metadata": { "kind": "compact_summary" }
                    });
                    let mut new_messages = vec![summary_msg];
                    new_messages.extend(result.messages.iter().cloned());
                    result.messages = new_messages;

                    // Store summary in boundary
                    if let Some(ref mut boundary) = result.boundary {
                        boundary.summary = Some(summary);
                    }
                }
                None => {
                    eprintln!(
                        "[compact_with_summary] summary generation failed (tier={:?}), using truncation only",
                        tier
                    );
                }
            }
        }
    }

    result
}


mod tests {
    use super::*;
    use serde_json::json;

    fn tool(content: &str) -> Value {
        json!({"role": "tool", "content": content})
    }
    fn user(content: &str) -> Value {
        json!({"role": "user", "content": content})
    }
    fn assistant(content: &str) -> Value {
        json!({"role": "assistant", "content": content})
    }

    #[test]
    fn normal_tier_no_compaction() {
        let msgs = vec![user("hello"), assistant("hi"), tool(&"x".repeat(5000))];
        let result = compact_tiered(&msgs, 100, 100, CompactionTier::Normal, 4);
        assert_eq!(result.len(), 3);
        // Content unchanged
        assert_eq!(
            result[2].get("content").unwrap().as_str().unwrap().len(),
            5000
        );
    }

    #[test]
    fn trim_schemas_tier_uses_double_keep() {
        // TrimSchemas uses keep_chars * 2 = 200
        let msgs = vec![
            tool(&"a".repeat(500)), // should be truncated to 200
            tool(&"b".repeat(100)), // last tool, not truncated
        ];
        let result = compact_tiered(&msgs, 50, 100, CompactionTier::TrimSchemas, 4);
        let first = result[0].get("content").unwrap().as_str().unwrap();
        assert!(first.contains("[compacted"), "should be compacted");
        assert!(first.len() < 500, "should be shorter than original");
    }

    #[test]
    fn compact_history_tier_original_behavior() {
        let msgs = vec![
            tool(&"a".repeat(5000)),
            tool(&"b".repeat(100)), // last tool preserved
        ];
        let result = compact_tiered(&msgs, 50, 2000, CompactionTier::CompactHistory, 4);
        let first = result[0].get("content").unwrap().as_str().unwrap();
        assert!(first.contains("[compacted"));
    }

    #[test]
    fn aggressive_prune_drops_old_turns() {
        let msgs = vec![
            user("old question 1"),
            assistant("old answer 1"),
            user("old question 2"),
            assistant("old answer 2"),
            user("recent question"),
            assistant("recent answer"),
            tool(&"x".repeat(100)),
        ];
        // keep_recent_turns=1 → keep last 2 conversation msgs (1 user + 1 assistant)
        let result = compact_tiered(&msgs, 10, 100, CompactionTier::AggressivePrune, 1);
        // Should have: recent user, recent assistant, tool = 3 messages
        assert_eq!(result.len(), 3, "should drop old turns, keep recent + tool");
        assert_eq!(
            result[0].get("content").unwrap().as_str().unwrap(),
            "recent question"
        );
    }

    #[test]
    fn aggressive_prune_halves_keep_chars() {
        let msgs = vec![
            tool(&"a".repeat(5000)),
            tool(&"b".repeat(100)), // last tool preserved
        ];
        // AggressivePrune uses keep_chars/2 = 500
        let result = compact_tiered(&msgs, 50, 1000, CompactionTier::AggressivePrune, 4);
        let first = result[0].get("content").unwrap().as_str().unwrap();
        assert!(first.contains("[compacted"));
        // Should be ~500 chars + compaction message
        assert!(first.len() < 600);
    }

    #[test]
    fn under_budget_no_compaction() {
        let msgs = vec![user("small"), tool("tiny")];
        let result = compact_tiered(&msgs, 100_000, 100, CompactionTier::AggressivePrune, 4);
        assert_eq!(result, msgs);
    }

    // --- CompactResult / CompactBoundary tests ---

    #[test]
    fn with_result_normal_tier_no_boundary() {
        let msgs = vec![user("hello"), tool("world")];
        let result = compact_tiered_with_result(&msgs, 100, 100, CompactionTier::Normal, 4);
        assert_eq!(result.tier, CompactionTier::Normal);
        assert!(result.boundary.is_none(), "Normal tier should produce no boundary");
        assert_eq!(result.messages.len(), 2);
    }

    #[test]
    fn with_result_under_budget_no_boundary() {
        let msgs = vec![user("hello"), tool("world")];
        let result =
            compact_tiered_with_result(&msgs, 100_000, 100, CompactionTier::AggressivePrune, 4);
        assert!(result.boundary.is_none(), "Under-budget should produce no boundary");
    }

    #[test]
    fn with_result_over_budget_has_boundary() {
        let msgs = vec![
            tool(&"a".repeat(5000)),
            tool(&"b".repeat(100)),
        ];
        let result =
            compact_tiered_with_result(&msgs, 50, 2000, CompactionTier::CompactHistory, 4);
        let boundary = result.boundary.expect("over-budget should produce boundary");
        assert_eq!(boundary.tier, CompactionTier::CompactHistory);
        assert_eq!(boundary.trigger, CompactTrigger::Auto);
        assert_eq!(boundary.messages_before, 2);
        assert_eq!(boundary.messages_after, result.messages.len());
    }

    #[test]
    fn boundary_to_system_message() {
        let boundary = CompactBoundary::new(CompactTrigger::Manual, CompactionTier::AggressivePrune)
            .with_pre_metrics(12000, 10)
            .with_post_count(4);
        let msg = boundary.to_system_message();
        assert_eq!(msg["role"].as_str().unwrap(), "system");
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("manually"));
        assert!(content.contains("aggressive-prune"));
        assert!(content.contains("10 → 4"));
    }

    #[test]
    fn boundary_serialization_round_trip() {
        let boundary = CompactBoundary::new(CompactTrigger::Auto, CompactionTier::TrimSchemas)
            .with_pre_metrics(5000, 8)
            .with_post_count(6)
            .with_last_uuid("abc-123")
            .with_recent_files(vec!["src/lib.rs".into()]);
        let json = serde_json::to_string(&boundary).unwrap();
        let restored: CompactBoundary = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.tier, CompactionTier::TrimSchemas);
        assert_eq!(restored.trigger, CompactTrigger::Auto);
        assert_eq!(restored.pre_tokens, 5000);
        assert_eq!(restored.messages_before, 8);
        assert_eq!(restored.messages_after, 6);
        assert_eq!(restored.last_pre_compact_uuid.as_deref(), Some("abc-123"));
        assert_eq!(restored.recent_files, vec!["src/lib.rs"]);
    }

    #[test]
    fn compact_tier_serialization() {
        for (tier, expected) in [
            (CompactionTier::Normal, "\"normal\""),
            (CompactionTier::TrimSchemas, "\"trim_schemas\""),
            (CompactionTier::CompactHistory, "\"compact_history\""),
            (CompactionTier::AggressivePrune, "\"aggressive_prune\""),
        ] {
            let s = serde_json::to_string(&tier).unwrap();
            assert_eq!(s, expected);
            let restored: CompactionTier = serde_json::from_str(&s).unwrap();
            assert_eq!(restored, tier);
        }
    }

    // --- CompactConfig tests ---

    #[test]
    fn compact_config_default_disables_summary() {
        let cfg = CompactConfig::default();
        assert!(!cfg.enable_summary);
        assert!(!cfg.should_summarize(CompactionTier::AggressivePrune));
    }

    #[test]
    fn compact_config_summary_enabled_respects_min_tier() {
        let cfg = CompactConfig {
            enable_summary: true,
            summary_min_tier: CompactionTier::AggressivePrune,
            ..Default::default()
        };
        assert!(!cfg.should_summarize(CompactionTier::Normal));
        assert!(!cfg.should_summarize(CompactionTier::TrimSchemas));
        assert!(!cfg.should_summarize(CompactionTier::CompactHistory));
        assert!(cfg.should_summarize(CompactionTier::AggressivePrune));
    }

    #[test]
    fn compact_config_lower_min_tier() {
        let cfg = CompactConfig {
            enable_summary: true,
            summary_min_tier: CompactionTier::CompactHistory,
            ..Default::default()
        };
        assert!(!cfg.should_summarize(CompactionTier::Normal));
        assert!(!cfg.should_summarize(CompactionTier::TrimSchemas));
        assert!(cfg.should_summarize(CompactionTier::CompactHistory));
        assert!(cfg.should_summarize(CompactionTier::AggressivePrune));
    }

    // --- compact_with_summary integration tests ---

    #[tokio::test]
    async fn compact_with_summary_disabled_no_summary_injected() {
        use crate::turn::cloud::summary::tests::MockSummaryClient;
        let client = MockSummaryClient::success("should not appear");
        let msgs = vec![
            tool(&"a".repeat(5000)),
            tool(&"b".repeat(100)),
        ];
        let cfg = CompactConfig { enable_summary: false, ..Default::default() };
        let result = compact_with_summary(&msgs, 50, 2000, CompactionTier::AggressivePrune, 4, &cfg, Some(&client)).await;
        // No summary message should be prepended
        let has_summary = result.messages.iter().any(|m| {
            m.get("attachment_metadata")
                .and_then(|a| a.get("kind"))
                .and_then(|k| k.as_str())
                == Some("compact_summary")
        });
        assert!(!has_summary);
    }

    #[tokio::test]
    async fn compact_with_summary_enabled_injects_summary() {
        use crate::turn::cloud::summary::tests::MockSummaryClient;
        let client = MockSummaryClient::success("## Task\nFix the bug");
        let msgs: Vec<Value> = (0..5)
            .flat_map(|i| vec![
                json!({"role": "user", "content": format!("q{i} {}", "x".repeat(200))}),
                json!({"role": "assistant", "content": format!("a{i} {}", "y".repeat(200))}),
            ])
            .collect();
        let cfg = CompactConfig {
            enable_summary: true,
            summary_min_tier: CompactionTier::AggressivePrune,
            ..Default::default()
        };
        let result = compact_with_summary(&msgs, 50, 100, CompactionTier::AggressivePrune, 1, &cfg, Some(&client)).await;
        // Summary message should be first
        let first = &result.messages[0];
        assert_eq!(
            first.get("attachment_metadata").and_then(|a| a.get("kind")).and_then(|k| k.as_str()),
            Some("compact_summary")
        );
        assert!(first["content"].as_str().unwrap().contains("Fix the bug"));
        // Boundary should contain summary
        let boundary = result.boundary.expect("should have boundary");
        assert!(boundary.summary.as_deref().unwrap().contains("Fix the bug"));
    }

    #[tokio::test]
    async fn compact_with_summary_fallback_on_no_client() {
        let msgs = vec![
            tool(&"a".repeat(5000)),
            tool(&"b".repeat(100)),
        ];
        let cfg = CompactConfig { enable_summary: true, ..Default::default() };
        // Pass None as client — should fall back to truncation silently
        let result = compact_with_summary(&msgs, 50, 2000, CompactionTier::AggressivePrune, 4, &cfg, None).await;
        assert!(result.boundary.is_some());
        // No summary
        assert!(result.boundary.unwrap().summary.is_none());
    }
}
