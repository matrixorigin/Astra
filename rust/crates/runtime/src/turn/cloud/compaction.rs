use crate::prompts::CompactionTier;
use serde_json::Value;

/// Tier-aware compaction: applies progressively more aggressive strategies.
///
/// * `Normal` — no compaction, return messages unchanged.
/// * `TrimSchemas` — compact only tool results longer than `keep_chars * 2`.
/// * `CompactHistory` — compact all tool results to `keep_chars` (original behavior).
/// * `AggressivePrune` — compact tool results to `keep_chars / 2`, then drop old
///   user/assistant pairs beyond `keep_recent_turns`.
pub fn compact_tiered(
    messages: &[Value],
    budget_chars: usize,
    keep_chars: usize,
    tier: CompactionTier,
    keep_recent_turns: usize,
) -> Vec<Value> {
    if tier == CompactionTier::Normal {
        return messages.to_vec();
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
        return messages.to_vec();
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

    compacted
}

#[cfg(test)]
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
}
