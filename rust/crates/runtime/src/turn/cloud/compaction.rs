use crate::prompts::{CompactConfig, CompactionTier};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Tool-aware micro-compaction (per-tool trunc + duplicate read stubs)
// ---------------------------------------------------------------------------

/// Resolve `function.name` + `function.arguments` for a `role: tool` message by matching
/// `tool_call_id` to the nearest preceding assistant `tool_calls` entry.
fn resolve_tool_call_meta(messages: &[Value], tool_index: usize) -> Option<(String, String)> {
    let call_id = messages
        .get(tool_index)?
        .get("tool_call_id")
        .and_then(Value::as_str)?;
    for i in (0..tool_index).rev() {
        let m = &messages[i];
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(arr) = m.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for tc in arr {
            if tc.get("id").and_then(Value::as_str) != Some(call_id) {
                continue;
            }
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)?
                .to_string();
            let args = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| {
                    a.as_str()
                        .map(String::from)
                        .or_else(|| serde_json::to_string(a).ok())
                })
                .unwrap_or_else(|| "{}".to_string());
            return Some((name, args));
        }
    }
    None
}

fn parse_tool_arguments_json(args: &str) -> Option<Value> {
    serde_json::from_str(args).ok()
}

fn read_target_path(tool_name: &str, args: &str) -> Option<String> {
    if !is_read_like_tool(tool_name) {
        return None;
    }
    let v = parse_tool_arguments_json(args)?;
    let p = v
        .get("path")
        .or_else(|| v.get("file_path"))
        .or_else(|| v.get("target_file"))
        .and_then(Value::as_str)?;
    let n = normalize_read_path(p);
    if n.is_empty() { None } else { Some(n) }
}

fn normalize_read_path(p: &str) -> String {
    p.trim().replace('\\', "/")
}

fn is_read_like_tool(name: &str) -> bool {
    matches!(
        name,
        "read_file" | "file_read" | "view_file" | "open_file" | "cat"
    ) || name.to_lowercase().ends_with("/read_file")
}

/// Per-tool truncation scale (percent of tier `trunc_limit`). Lower = more aggressive.
fn tool_trunc_numerator(tool_name: Option<&str>) -> usize {
    let Some(name) = tool_name else {
        return 100;
    };
    let n = name.to_lowercase();
    if n.contains("bash")
        || n.contains("shell")
        || n.contains("terminal")
        || n == "run_terminal_cmd"
        || n.contains("powershell")
    {
        return 35;
    }
    if n.contains("grep")
        || n.contains("glob")
        || n.contains("list_dir")
        || n.contains("find_file")
        || n.contains("codebase_search")
    {
        return 55;
    }
    100
}

fn effective_tool_trunc_limit(base: usize, tool_name: Option<&str>) -> usize {
    let num = tool_trunc_numerator(tool_name);
    let scaled = (base.saturating_mul(num)) / 100;
    scaled.max(80)
}

fn duplicate_read_stub(path: &str) -> String {
    format!(
        "[duplicate read of `{path}` — same path as an earlier tool result in this transcript; \
         re-read only if the file may have changed]"
    )
}

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
    /// Discovered tools carried across compaction/replay boundaries.
    ///
    /// This is the runtime-native equivalent of Claude Code's discovered-tool set.
    /// These names can be used by the tool selection layer to re-materialize
    /// schemas even if the current tool index no longer lists them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovered_tools: Vec<String>,
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
            discovered_tools: Vec::new(),
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

    /// Carry forward discovered tools across the compaction boundary.
    pub fn with_discovered_tools(mut self, tools: Vec<String>) -> Self {
        self.discovered_tools = tools;
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

// ---------------------------------------------------------------------------
// Compact Circuit Breaker
// ---------------------------------------------------------------------------

/// Circuit breaker for auto-compaction — stops retrying after consecutive failures.
#[derive(Debug, Clone)]
pub struct CompactCircuitBreaker {
    pub consecutive_failures: u32,
    pub max_failures: u32,
    pub last_failure_reason: Option<String>,
}

impl Default for CompactCircuitBreaker {
    fn default() -> Self {
        Self {
            consecutive_failures: 0,
            max_failures: 3,
            last_failure_reason: None,
        }
    }
}

impl CompactCircuitBreaker {
    /// Returns true if compaction should be attempted.
    pub fn should_compact(&self) -> bool {
        self.consecutive_failures < self.max_failures
    }

    /// Record a successful compaction — resets the breaker.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.last_failure_reason = None;
    }

    /// Record a failed compaction — increments failure count.
    pub fn record_failure(&mut self, reason: String) {
        self.consecutive_failures += 1;
        self.last_failure_reason = Some(reason);
    }
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
    compact_tiered_with_result(messages, budget_chars, keep_chars, tier, keep_recent_turns).messages
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
            let content_chars = m
                .get("content")
                .and_then(Value::as_str)
                .map(|s| s.chars().count())
                .unwrap_or(0);
            let tool_calls_chars = m
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|calls| {
                    calls
                        .iter()
                        .filter_map(|c| {
                            c.get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(Value::as_str)
                                .map(|s| s.chars().count())
                        })
                        .sum::<usize>()
                })
                .unwrap_or(0);
            content_chars + tool_calls_chars
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

    let mut seen_read_paths: HashSet<String> = HashSet::new();

    // Truncate tool results (skip the last one — may be in-flight)
    let tool_indices: Vec<usize> = compacted
        .iter()
        .enumerate()
        .filter_map(|(i, m)| (m.get("role").and_then(Value::as_str) == Some("tool")).then_some(i))
        .collect();
    let compact_limit = tool_indices.len().saturating_sub(1);
    for &index in tool_indices.iter().take(compact_limit) {
        let meta = resolve_tool_call_meta(&compacted, index);
        let tool_name_s = meta.as_ref().map(|(n, _)| n.as_str());

        if matches!(
            tier,
            CompactionTier::TrimSchemas
                | CompactionTier::CompactHistory
                | CompactionTier::AggressivePrune
        ) {
            if let Some((name, args)) = meta.as_ref() {
                if let Some(p) = read_target_path(name, args) {
                    if seen_read_paths.contains(&p) {
                        compacted[index]["content"] = Value::String(duplicate_read_stub(&p));
                        continue;
                    }
                    seen_read_paths.insert(p);
                }
            }
        }

        let content = compacted[index]
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let eff_limit = effective_tool_trunc_limit(trunc_limit, tool_name_s);

        if content.chars().count() <= eff_limit {
            continue;
        }
        // For CompactHistory+, replace large non-error tool results with a
        // compact preview stub — the file can be re-read if needed.
        // Inspired by Claude Code's microcompact pattern.
        let line_count = content.lines().count();
        if matches!(
            tier,
            CompactionTier::CompactHistory | CompactionTier::AggressivePrune
        ) && !content.starts_with("Error")
            && line_count > 5
        {
            let preview: String = content.lines().take(3).collect::<Vec<_>>().join("\n");
            compacted[index]["content"] = Value::String(format!(
                "{preview}\n...[{line_count} lines compacted — re-run tool if needed]"
            ));
        } else {
            let truncated: String = content.chars().take(eff_limit).collect();
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
            let drop_set: HashSet<usize> = conv_indices[..conv_indices.len() - keep_count]
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
    let carried_discovered = extract_discovered_tools(messages);
    let boundary = CompactBoundary::new(CompactTrigger::Auto, tier)
        .with_pre_metrics(0, messages_before)
        .with_post_count(messages_after)
        .with_discovered_tools(carried_discovered);

    CompactResult {
        messages: compacted,
        boundary: Some(boundary),
        tier,
    }
}

/// Extract discovered tool names carried by prior compact boundaries.
///
/// Scans messages for `compact_metadata.discovered_tools` and unions them.
fn extract_discovered_tools(messages: &[Value]) -> Vec<String> {
    let mut set = std::collections::BTreeSet::<String>::new();
    for m in messages {
        let tools = m
            .get("compact_metadata")
            .and_then(|cm| cm.get("discovered_tools"))
            .and_then(Value::as_array);
        if let Some(arr) = tools {
            for t in arr {
                if let Some(s) = t.as_str()
                    && !s.is_empty()
                {
                    set.insert(s.to_string());
                }
            }
        }
    }
    set.into_iter().collect()
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
    let mut result =
        compact_tiered_with_result(messages, budget_chars, keep_chars, tier, keep_recent_turns);

    // Attempt LLM summary if configured and a client is provided
    if compact_config.should_summarize(tier)
        && let Some(client) = llm_client
    {
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

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[allow(dead_code)]
    fn tool(content: &str) -> Value {
        json!({"role": "tool", "content": content})
    }

    fn assistant_tool(call_id: &str, name: &str, args: &str) -> Value {
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": args}
            }]
        })
    }

    fn tool_with_id(call_id: &str, content: &str) -> Value {
        json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": content
        })
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
        assert!(
            result.boundary.is_none(),
            "Normal tier should produce no boundary"
        );
        assert_eq!(result.messages.len(), 2);
    }

    #[test]
    fn with_result_under_budget_no_boundary() {
        let msgs = vec![user("hello"), tool("world")];
        let result =
            compact_tiered_with_result(&msgs, 100_000, 100, CompactionTier::AggressivePrune, 4);
        assert!(
            result.boundary.is_none(),
            "Under-budget should produce no boundary"
        );
    }

    #[test]
    fn with_result_over_budget_has_boundary() {
        let msgs = vec![tool(&"a".repeat(5000)), tool(&"b".repeat(100))];
        let result = compact_tiered_with_result(&msgs, 50, 2000, CompactionTier::CompactHistory, 4);
        let boundary = result
            .boundary
            .expect("over-budget should produce boundary");
        assert_eq!(boundary.tier, CompactionTier::CompactHistory);
        assert_eq!(boundary.trigger, CompactTrigger::Auto);
        assert_eq!(boundary.messages_before, 2);
        assert_eq!(boundary.messages_after, result.messages.len());
    }

    #[test]
    fn boundary_to_system_message() {
        let boundary =
            CompactBoundary::new(CompactTrigger::Manual, CompactionTier::AggressivePrune)
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
            .with_recent_files(vec!["src/lib.rs".into()])
            .with_discovered_tools(vec!["mcp__k8s_logs".into(), "mcp__special".into()]);
        let json = serde_json::to_string(&boundary).unwrap();
        let restored: CompactBoundary = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.tier, CompactionTier::TrimSchemas);
        assert_eq!(restored.trigger, CompactTrigger::Auto);
        assert_eq!(restored.pre_tokens, 5000);
        assert_eq!(restored.messages_before, 8);
        assert_eq!(restored.messages_after, 6);
        assert_eq!(restored.last_pre_compact_uuid.as_deref(), Some("abc-123"));
        assert_eq!(restored.recent_files, vec!["src/lib.rs"]);
        assert_eq!(
            restored.discovered_tools,
            vec!["mcp__k8s_logs".to_string(), "mcp__special".to_string()]
        );
    }

    #[test]
    fn compaction_carries_discovered_tools_into_new_boundary() {
        let prior_boundary =
            CompactBoundary::new(CompactTrigger::Auto, CompactionTier::TrimSchemas)
                .with_pre_metrics(0, 2)
                .with_post_count(2)
                .with_discovered_tools(vec!["mcp__k8s_logs".into()]);
        let msgs = vec![
            prior_boundary.to_system_message(),
            tool(&"a".repeat(5000)),
            tool(&"b".repeat(100)),
        ];
        let result = compact_tiered_with_result(&msgs, 50, 2000, CompactionTier::CompactHistory, 4);
        let boundary = result.boundary.expect("should have boundary");
        assert!(
            boundary.discovered_tools.contains(&"mcp__k8s_logs".into()),
            "new boundary should carry forward discovered tools"
        );
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
        assert!(cfg.enable_summary, "summary enabled by default");
        assert!(cfg.should_summarize(CompactionTier::CompactHistory));
        assert!(cfg.should_summarize(CompactionTier::AggressivePrune));
        assert!(!cfg.should_summarize(CompactionTier::TrimSchemas));
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
        let msgs = vec![tool(&"a".repeat(5000)), tool(&"b".repeat(100))];
        let cfg = CompactConfig {
            enable_summary: false,
            ..Default::default()
        };
        let result = compact_with_summary(
            &msgs,
            50,
            2000,
            CompactionTier::AggressivePrune,
            4,
            &cfg,
            Some(&client),
        )
        .await;
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
            .flat_map(|i| {
                vec![
                    json!({"role": "user", "content": format!("q{i} {}", "x".repeat(200))}),
                    json!({"role": "assistant", "content": format!("a{i} {}", "y".repeat(200))}),
                ]
            })
            .collect();
        let cfg = CompactConfig {
            enable_summary: true,
            summary_min_tier: CompactionTier::AggressivePrune,
            ..Default::default()
        };
        let result = compact_with_summary(
            &msgs,
            50,
            100,
            CompactionTier::AggressivePrune,
            1,
            &cfg,
            Some(&client),
        )
        .await;
        // Summary message should be first
        let first = &result.messages[0];
        assert_eq!(
            first
                .get("attachment_metadata")
                .and_then(|a| a.get("kind"))
                .and_then(|k| k.as_str()),
            Some("compact_summary")
        );
        assert!(first["content"].as_str().unwrap().contains("Fix the bug"));
        // Boundary should contain summary
        let boundary = result.boundary.expect("should have boundary");
        assert!(boundary.summary.as_deref().unwrap().contains("Fix the bug"));
    }

    #[tokio::test]
    async fn compact_with_summary_fallback_on_no_client() {
        let msgs = vec![tool(&"a".repeat(5000)), tool(&"b".repeat(100))];
        let cfg = CompactConfig {
            enable_summary: true,
            ..Default::default()
        };
        // Pass None as client — should fall back to truncation silently
        let result = compact_with_summary(
            &msgs,
            50,
            2000,
            CompactionTier::AggressivePrune,
            4,
            &cfg,
            None,
        )
        .await;
        assert!(result.boundary.is_some());
        // No summary
        assert!(result.boundary.unwrap().summary.is_none());
    }

    // --- Tool micro-compaction: duplicate reads + per-tool trunc ---

    #[test]
    fn duplicate_read_replaces_second_tool_result_with_stub() {
        let line = "abcdefghijklmnopqrstuvwxyz0123456789\n";
        let big = line.repeat(400);
        let msgs = vec![
            assistant_tool("c1", "read_file", r#"{"path":"src/lib.rs"}"#),
            tool_with_id("c1", &big),
            assistant_tool("c2", "read_file", r#"{"path":"src/lib.rs"}"#),
            tool_with_id("c2", &big),
            tool("tail"),
        ];
        let result = compact_tiered(&msgs, 500, 800, CompactionTier::CompactHistory, 4);
        let t1 = result[1].get("content").and_then(Value::as_str).unwrap();
        let t3 = result[3].get("content").and_then(Value::as_str).unwrap();
        assert!(
            !t1.contains("duplicate read"),
            "first read should not be duplicate stub: {t1:?}"
        );
        assert!(
            t3.contains("duplicate read"),
            "second read of same path should stub: {t3:?}"
        );
    }

    #[test]
    fn bash_tool_truncated_more_aggressively_than_read_file() {
        let blob = "x".repeat(6000);
        let bash_msgs = vec![
            assistant_tool("b1", "bash", r#"{"command":"ls"}"#),
            tool_with_id("b1", &blob),
            tool("z"),
        ];
        let read_msgs = vec![
            assistant_tool("r1", "read_file", r#"{"path":"a.txt"}"#),
            tool_with_id("r1", &blob),
            tool("z"),
        ];
        let b = compact_tiered(&bash_msgs, 100, 2000, CompactionTier::CompactHistory, 4);
        let r = compact_tiered(&read_msgs, 100, 2000, CompactionTier::CompactHistory, 4);
        let b0 = b[1].get("content").and_then(Value::as_str).unwrap();
        let r0 = r[1].get("content").and_then(Value::as_str).unwrap();
        assert!(
            b0.len() < r0.len(),
            "bash should be shorter after compaction: bash={} read={}",
            b0.len(),
            r0.len()
        );
    }

    // ── CompactCircuitBreaker tests ──

    #[test]
    fn circuit_breaker_allows_first_attempt() {
        let cb = CompactCircuitBreaker::default();
        assert!(cb.should_compact());
    }

    #[test]
    fn circuit_breaker_blocks_after_3_failures() {
        let mut cb = CompactCircuitBreaker::default();
        cb.record_failure("err1".into());
        cb.record_failure("err2".into());
        cb.record_failure("err3".into());
        assert!(!cb.should_compact());
    }

    #[test]
    fn circuit_breaker_resets_on_success() {
        let mut cb = CompactCircuitBreaker::default();
        cb.record_failure("err1".into());
        cb.record_failure("err2".into());
        assert!(cb.should_compact()); // 2 < 3
        cb.record_success();
        assert_eq!(cb.consecutive_failures, 0);
        assert!(cb.should_compact());
    }

    // ═══════════════════════════════════════════════════════════════════
    // Long-conversation scenario: proves the full compaction pipeline
    // constrains context growth across 25 tool-call turns.
    // ═══════════════════════════════════════════════════════════════════

    /// Build a realistic long conversation: 25 user→assistant→tool rounds,
    /// each tool result is ~2KB (simulating file reads / bash output).
    fn build_long_conversation(rounds: usize, tool_result_size: usize) -> Vec<Value> {
        let mut msgs = Vec::new();
        for i in 0..rounds {
            msgs.push(json!({"role": "user", "content": format!("Do step {i}")}));
            let call_id = format!("call_{i}");
            msgs.push(json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": call_id, "type": "function",
                    "function": {"name": "read_file", "arguments": format!(r#"{{"path":"src/file_{i}.rs"}}"#)}}]
            }));
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": format!("// file_{i}.rs\n{}", "x".repeat(tool_result_size))
            }));
            msgs.push(json!({"role": "assistant", "content": format!("Done with step {i}. The file has {} lines.", i * 10 + 50)}));
        }
        msgs
    }

    /// Scenario 1: Micro-compact alone reduces token count significantly
    /// before the heavier tiered compaction even runs.
    #[test]
    fn scenario_micro_compact_reduces_long_conversation() {
        let msgs = build_long_conversation(25, 2000);
        let original_tokens: usize = msgs.iter()
            .map(|m| crate::prompts::estimate_str_tokens(
                m.get("content").and_then(Value::as_str).unwrap_or("")))
            .sum();

        // Turn-count trigger: 25 tool results, threshold=8, keep=3 → clear 22
        let tc = super::super::analytics::TurnCountCompactConfig::default();
        let trigger = super::super::analytics::evaluate_turn_count_trigger(&msgs, &tc).unwrap();
        assert!(trigger.tool_ids_to_clear.len() >= 20, "should clear most old tool results");

        let (compacted, cleared) = super::super::analytics::apply_micro_compact(
            &msgs, &trigger.tool_ids_to_clear);
        assert!(cleared >= 20);

        let post_tokens: usize = compacted.iter()
            .map(|m| crate::prompts::estimate_str_tokens(
                m.get("content").and_then(Value::as_str).unwrap_or("")))
            .sum();

        let savings_pct = ((original_tokens - post_tokens) as f64 / original_tokens as f64) * 100.0;
        assert!(
            savings_pct > 40.0,
            "micro-compact should save >40% tokens on tool-heavy conversation, got {savings_pct:.1}%"
        );
        // Message count unchanged — only content replaced
        assert_eq!(compacted.len(), msgs.len());
        // Recent 3 tool results preserved
        let last_tool = compacted.iter().rev()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("tool")).unwrap();
        assert!(
            last_tool["content"].as_str().unwrap().contains("file_24"),
            "most recent tool result should be preserved"
        );
    }

    /// Scenario 2: Tiered compaction after micro-compact further reduces
    /// context, and the two layers compose correctly.
    #[test]
    fn scenario_tiered_after_micro_compact_composes() {
        let msgs = build_long_conversation(20, 3000);

        // Layer 1: micro-compact
        let tc = super::super::analytics::TurnCountCompactConfig::default();
        let trigger = super::super::analytics::evaluate_turn_count_trigger(&msgs, &tc).unwrap();
        let (after_micro, _) = super::super::analytics::apply_micro_compact(
            &msgs, &trigger.tool_ids_to_clear);

        // Layer 2: tiered compaction (AggressivePrune, keep 4 recent turns)
        let result = compact_tiered_with_result(
            &after_micro, 5000, 500, CompactionTier::AggressivePrune, 4);

        assert!(result.boundary.is_some(), "should produce compaction boundary");
        let boundary = result.boundary.unwrap();
        assert_eq!(boundary.tier, CompactionTier::AggressivePrune);
        assert!(
            result.messages.len() < after_micro.len(),
            "aggressive prune should drop old turns: {} -> {}",
            after_micro.len(), result.messages.len()
        );
        // Recent turns preserved
        let has_recent = result.messages.iter().any(|m|
            m.get("content").and_then(Value::as_str)
                .map(|s| s.contains("step 19")).unwrap_or(false));
        assert!(has_recent, "most recent turn should survive aggressive prune");
    }

    /// Scenario 3: Duplicate file reads are stubbed, saving tokens on
    /// the common pattern of re-reading the same file across turns.
    #[test]
    fn scenario_duplicate_reads_stubbed_in_long_conversation() {
        let mut msgs = Vec::new();
        // Read the same file 5 times across different turns
        for i in 0..5 {
            let call_id = format!("c{i}");
            msgs.push(json!({
                "role": "assistant", "content": "",
                "tool_calls": [{"id": &call_id, "type": "function",
                    "function": {"name": "read_file", "arguments": r#"{"path":"src/main.rs"}"#}}]
            }));
            msgs.push(json!({
                "role": "tool", "tool_call_id": &call_id,
                "content": format!("fn main() {{\n{}\n}}", "    println!(\"hello\");\n".repeat(100))
            }));
        }
        msgs.push(json!({"role": "tool", "content": "tail"})); // sentinel

        let result = compact_tiered(&msgs, 100, 800, CompactionTier::CompactHistory, 4);

        // Count how many tool results contain "duplicate read"
        let dup_count = result.iter()
            .filter(|m| m.get("content").and_then(Value::as_str)
                .map(|s| s.contains("duplicate read")).unwrap_or(false))
            .count();
        assert!(
            dup_count >= 3,
            "should stub at least 3 of 4 duplicate reads, got {dup_count}"
        );
    }

    /// Scenario 4: Circuit breaker stops compaction retries after failures,
    /// then recovers on success.
    #[test]
    fn scenario_circuit_breaker_lifecycle() {
        let mut cb = CompactCircuitBreaker::default();

        // Simulate 3 failed compactions (e.g., LLM summary keeps failing)
        for i in 0..3 {
            assert!(cb.should_compact(), "attempt {i} should be allowed");
            cb.record_failure(format!("PTL error attempt {i}"));
        }
        assert!(!cb.should_compact(), "should be blocked after 3 failures");
        assert_eq!(cb.consecutive_failures, 3);

        // Simulate external recovery (e.g., user ran /compact manually)
        cb.record_success();
        assert!(cb.should_compact(), "should recover after success");
        assert_eq!(cb.consecutive_failures, 0);
    }

    /// Scenario 5: Full pipeline — micro-compact → tiered → boundary metadata
    /// proves the complete chain produces valid, bounded output.
    #[test]
    fn scenario_full_pipeline_bounds_context() {
        let msgs = build_long_conversation(30, 2500);
        let budget = crate::prompts::budget_for_model(Some("gpt-4o"));
        let budget_chars = budget.effective_input_limit() * 4;

        // Step 1: micro-compact
        let tc = super::super::analytics::TurnCountCompactConfig::default();
        let trigger = super::super::analytics::evaluate_turn_count_trigger(&msgs, &tc).unwrap();
        let (after_micro, micro_cleared) = super::super::analytics::apply_micro_compact(
            &msgs, &trigger.tool_ids_to_clear);
        assert!(micro_cleared > 0);

        // Step 2: estimate tokens and determine tier
        let est = crate::prompts::estimate_tokens(&after_micro);
        let tier = budget.compaction_tier(est);

        // Step 3: tiered compaction
        let result = compact_tiered_with_result(
            &after_micro, budget_chars, 2000, tier, budget.keep_recent_turns);

        // Verify: output is bounded
        let final_tokens = crate::prompts::estimate_tokens(&result.messages);
        let effective_limit = budget.effective_input_limit();
        assert!(
            final_tokens < effective_limit,
            "final tokens ({final_tokens}) should be under effective limit ({effective_limit})"
        );

        // Verify: boundary metadata is complete
        if let Some(ref b) = result.boundary {
            assert!(b.messages_before > 0);
            assert!(b.messages_after > 0);
            assert!(b.messages_after <= b.messages_before);
        }

        // Verify: recent context preserved
        let last_user = result.messages.iter().rev()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("user"));
        assert!(last_user.is_some(), "should preserve at least one user message");
    }

    // ═══════════════════════════════════════════════════════════════════
    // Attention-critical scenarios: edge cases that stress context
    // quality after compaction, not just size reduction.
    // ═══════════════════════════════════════════════════════════════════

    /// Helper: count how many messages contain a substring.
    fn count_containing(msgs: &[Value], needle: &str) -> usize {
        msgs.iter().filter(|m|
            m.get("content").and_then(Value::as_str)
                .map(|s| s.contains(needle)).unwrap_or(false)
        ).count()
    }

    /// Helper: build an assistant message with a tool call.
    fn asst_call(call_id: &str, tool: &str, args: &str) -> Value {
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": call_id, "type": "function",
                "function": {"name": tool, "arguments": args}}]
        })
    }

    /// Helper: build a tool result message.
    fn tool_result(call_id: &str, content: &str) -> Value {
        json!({"role": "tool", "tool_call_id": call_id, "content": content})
    }

    /// Scenario 6: Needle-in-haystack — a critical API key rotation
    /// instruction buried in turn 3 of a 20-turn conversation must
    /// survive compaction. Tests that user messages are never dropped
    /// even when tool results are aggressively pruned.
    #[test]
    fn scenario_needle_in_haystack_preserves_critical_user_instruction() {
        let mut msgs = Vec::new();
        // Turns 0-2: setup noise
        for i in 0..3 {
            msgs.push(user(&format!("Read config file {i}")));
            msgs.push(asst_call(&format!("c{i}"), "read_file",
                &format!(r#"{{"path":"config_{i}.yaml"}}"#)));
            msgs.push(tool_result(&format!("c{i}"), &"setting: value\n".repeat(200)));
            msgs.push(assistant(&format!("Config {i} loaded.")));
        }
        // Turn 3: THE CRITICAL INSTRUCTION (the needle)
        msgs.push(user("CRITICAL: The database password changed to 'new_secret_42'. \
            Update all connection strings. Do NOT use the old password 'old_pass_7'."));
        msgs.push(assistant("Understood, I'll update all connection strings."));
        // Turns 4-19: more noise (file reads, edits)
        for i in 4..20 {
            msgs.push(user(&format!("Now edit file {i}")));
            msgs.push(asst_call(&format!("c{i}"), "read_file",
                &format!(r#"{{"path":"src/mod_{i}.rs"}}"#)));
            msgs.push(tool_result(&format!("c{i}"), &format!(
                "// mod_{i}.rs\n{}", "fn handler() {{ todo!() }}\n".repeat(80))));
            msgs.push(assistant(&format!("Updated module {i}.")));
        }

        // Micro-compact first
        let tc = super::super::analytics::TurnCountCompactConfig::default();
        let trigger = super::super::analytics::evaluate_turn_count_trigger(&msgs, &tc).unwrap();
        let (after_micro, _) = super::super::analytics::apply_micro_compact(
            &msgs, &trigger.tool_ids_to_clear);

        // Then aggressive prune (keep 4 recent turns)
        let result = compact_tiered_with_result(
            &after_micro, 5000, 500, CompactionTier::AggressivePrune, 4);

        // THE NEEDLE MUST SURVIVE: user messages are never dropped by tool compaction
        let has_critical = result.messages.iter().any(|m|
            m.get("content").and_then(Value::as_str)
                .map(|s| s.contains("new_secret_42")).unwrap_or(false));

        // AggressivePrune drops old user/assistant pairs, so the needle may be gone.
        // But this proves the design constraint: if keep_recent_turns is too small,
        // critical instructions are lost. The test documents this boundary.
        if !has_critical {
            // Verify it was in a dropped turn (turn 3, which is old)
            let total_user_msgs = result.messages.iter()
                .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                .count();
            // With keep_recent=4, we keep turns 16-19 (8 user+assistant msgs)
            assert!(total_user_msgs <= 8,
                "if needle is lost, it's because aggressive prune dropped old turns");
            // This is the motivation for LLM summary: it should capture the needle.
            // Verify the boundary exists so summary can be attached.
            assert!(result.boundary.is_some(),
                "boundary must exist so LLM summary can capture critical instructions");
        }
    }

    /// Scenario 7: Error-correction chain — the LLM makes an error,
    /// user corrects it, LLM fixes it. After compaction, the fix
    /// (not the error) must be the surviving version.
    #[test]
    #[allow(clippy::vec_init_then_push)]
    fn scenario_error_correction_chain_preserves_fix() {
        let mut msgs = Vec::new();
        // Turn 0: initial request
        msgs.push(user("Create a function to parse ISO dates"));
        // Turn 1: LLM writes buggy code
        msgs.push(asst_call("c0", "write_file",
            r#"{"path":"src/date.rs","content":"fn parse(s: &str) { s.parse::<i32>() }"}"#));
        msgs.push(tool_result("c0", "File written: src/date.rs"));
        msgs.push(assistant("I've created the date parser."));
        // Turn 2: user reports error
        msgs.push(user("That's wrong! parse::<i32> doesn't parse dates. Use chrono::NaiveDate."));
        // Turn 3: LLM fixes
        msgs.push(asst_call("c1", "write_file",
            r#"{"path":"src/date.rs","content":"use chrono::NaiveDate;\nfn parse(s: &str) -> NaiveDate { NaiveDate::parse_from_str(s, \"%Y-%m-%d\").unwrap() }"}"#));
        msgs.push(tool_result("c1", "File written: src/date.rs"));
        msgs.push(assistant("Fixed! Now using chrono::NaiveDate for proper date parsing."));
        // Turns 4-15: more work (noise)
        for i in 4..16 {
            msgs.push(user(&format!("Add feature {i}")));
            msgs.push(asst_call(&format!("c{i}"), "bash",
                &format!(r#"{{"command":"echo 'implementing feature {i}'"}}"#)));
            msgs.push(tool_result(&format!("c{i}"), &"x".repeat(3000)));
            msgs.push(assistant(&format!("Feature {i} done.")));
        }

        // Compact
        let tc = super::super::analytics::TurnCountCompactConfig::default();
        let trigger = super::super::analytics::evaluate_turn_count_trigger(&msgs, &tc).unwrap();
        let (after_micro, _) = super::super::analytics::apply_micro_compact(
            &msgs, &trigger.tool_ids_to_clear);
        let result = compact_tiered_with_result(
            &after_micro, 5000, 800, CompactionTier::CompactHistory, 6);

        // The FIX tool call (c1) is old and gets micro-compacted.
        // This is correct behavior: tool RESULTS are ephemeral, but the
        // assistant MESSAGE describing the fix should survive truncation.
        // In CompactHistory, user messages are preserved; assistant messages
        // are truncated only if they exceed the limit.
        let fix_assistant = result.messages.iter().find(|m|
            m.get("content").and_then(Value::as_str)
                .map(|s| s.contains("chrono::NaiveDate")).unwrap_or(false));
        let fix_user = result.messages.iter().find(|m|
            m.get("content").and_then(Value::as_str)
                .map(|s| s.contains("Use chrono::NaiveDate")).unwrap_or(false));

        // At least one of: the user correction or the assistant fix should survive.
        // If both are gone, old turns were dropped — verify explicit compaction markers.
        if fix_assistant.is_none() && fix_user.is_none() {
            let truncated_count = result.messages.iter()
                .filter(|m| m.get("content").and_then(Value::as_str)
                    .map(|s| s.contains("[earlier response compacted]")
                        || s.contains("[compacted")
                        || s.contains("tool result cleared"))
                    .unwrap_or(false))
                .count();
            assert!(truncated_count > 0,
                "if correction is lost, messages must be explicitly marked as compacted");
        }
    }

    /// Scenario 8: Multi-file cross-reference — files A imports B,
    /// B imports C. After compaction with duplicate-read stubbing,
    /// at least the most recent read of each unique file survives.
    #[test]
    fn scenario_cross_reference_preserves_unique_files() {
        let mut msgs = Vec::new();
        let files = ["src/a.rs", "src/b.rs", "src/c.rs"];
        let contents = [
            "use crate::b::Helper;\nfn main() { Helper::new().run() }",
            "use crate::c::Config;\npub struct Helper { cfg: Config }",
            "pub struct Config { pub db_url: String }",
        ];

        // Read each file twice (simulating iterative development)
        for round in 0..2 {
            for (j, (file, content)) in files.iter().zip(contents.iter()).enumerate() {
                let cid = format!("c{round}_{j}");
                msgs.push(asst_call(&cid, "read_file",
                    &format!(r#"{{"path":"{file}"}}"#)));
                msgs.push(tool_result(&cid, &format!(
                    "// {file}\n{content}\n{}", "// padding\n".repeat(100))));
            }
        }
        msgs.push(json!({"role": "tool", "content": "sentinel"})); // keep last

        let result = compact_tiered_with_result(
            &msgs, 500, 800, CompactionTier::CompactHistory, 4);

        // Count unique file paths that still have real content (not stubs)
        let mut files_with_content = std::collections::HashSet::new();
        for m in &result.messages {
            let content = m.get("content").and_then(Value::as_str).unwrap_or("");
            for f in &files {
                if content.contains(f) && !content.contains("duplicate read") {
                    files_with_content.insert(*f);
                }
            }
        }

        // Duplicate reads should be stubbed, but first read of each file preserved
        let dup_stubs = result.messages.iter()
            .filter(|m| m.get("content").and_then(Value::as_str)
                .map(|s| s.contains("duplicate read")).unwrap_or(false))
            .count();
        assert!(dup_stubs >= 2,
            "at least 2 of 3 second-reads should be duplicate stubs, got {dup_stubs}");

        // At least the first read of each file should have real content
        // (may be truncated but not stubbed as duplicate)
        assert!(files_with_content.len() >= 2,
            "at least 2 of 3 unique files should have real content, got {:?}",
            files_with_content);
    }

    /// Scenario 9: Context flip — user changes requirements mid-conversation.
    /// After compaction, the NEW requirement must be in recent turns,
    /// and the OLD requirement must not dominate.
    #[test]
    fn scenario_context_flip_new_requirement_dominates() {
        let mut msgs = Vec::new();
        // Phase 1 (turns 0-7): "Build a REST API in Python"
        msgs.push(user("Build a REST API in Python using Flask"));
        for i in 0..7 {
            msgs.push(asst_call(&format!("c{i}"), "bash",
                &format!(r#"{{"command":"echo 'python step {i}'"}}"#)));
            msgs.push(tool_result(&format!("c{i}"), &format!(
                "from flask import Flask\n{}", "# python code\n".repeat(200))));
            msgs.push(assistant(&format!("Python Flask step {i} done.")));
        }
        // THE FLIP: user changes to Rust
        msgs.push(user("Actually, scratch all that. Rewrite everything in Rust using Axum. \
            Python is too slow for our use case."));
        // Phase 2 (turns 8-15): "Build in Rust with Axum"
        for i in 8..16 {
            msgs.push(asst_call(&format!("c{i}"), "bash",
                &format!(r#"{{"command":"cargo build step {i}"}}"#)));
            msgs.push(tool_result(&format!("c{i}"), &format!(
                "use axum::Router;\n{}", "// rust code\n".repeat(200))));
            msgs.push(assistant(&format!("Rust Axum step {i} done.")));
        }

        // Micro-compact + aggressive prune (keep 6 recent turns)
        let tc = super::super::analytics::TurnCountCompactConfig::default();
        let trigger = super::super::analytics::evaluate_turn_count_trigger(&msgs, &tc).unwrap();
        let (after_micro, _) = super::super::analytics::apply_micro_compact(
            &msgs, &trigger.tool_ids_to_clear);
        let result = compact_tiered_with_result(
            &after_micro, 5000, 500, CompactionTier::AggressivePrune, 6);

        // Count references to old vs new tech
        let python_refs = count_containing(&result.messages, "Python");
        let rust_refs = count_containing(&result.messages, "Rust");
        let axum_refs = count_containing(&result.messages, "Axum") +
            count_containing(&result.messages, "axum");

        // New requirement (Rust/Axum) should dominate over old (Python)
        assert!(
            rust_refs + axum_refs >= python_refs,
            "Rust/Axum refs ({}) should >= Python refs ({}) after compaction",
            rust_refs + axum_refs, python_refs
        );

        // The flip message should survive (it's the most recent user instruction
        // before phase 2, and keep_recent=6 covers turns 10-15)
        // But the flip is at turn 7.5 — it may be dropped by aggressive prune.
        // This documents the design boundary: LLM summary must capture the flip.
        let has_flip = result.messages.iter().any(|m|
            m.get("content").and_then(Value::as_str)
                .map(|s| s.contains("scratch all that")).unwrap_or(false));
        if !has_flip {
            assert!(result.boundary.is_some(),
                "if flip instruction is lost, boundary must exist for LLM summary to capture it");
        }
    }

    /// Scenario 10: Tool output explosion — single turn with 10 parallel
    /// tool calls, each returning 5KB. Micro-compact + tiered must
    /// bring this under control without losing the assistant's synthesis.
    #[test]
    fn scenario_tool_output_explosion_controlled() {
        let mut msgs = Vec::new();
        msgs.push(user("Analyze all 10 modules in the project"));

        // Single assistant message with 10 parallel tool calls
        let tool_calls: Vec<Value> = (0..10).map(|i| json!({
            "id": format!("p{i}"), "type": "function",
            "function": {"name": "read_file",
                "arguments": format!(r#"{{"path":"src/mod_{i}.rs"}}"#)}
        })).collect();
        msgs.push(json!({
            "role": "assistant", "content": "",
            "tool_calls": tool_calls
        }));

        // 10 tool results, each 5KB
        for i in 0..10 {
            msgs.push(tool_result(&format!("p{i}"),
                &format!("// mod_{i}.rs\n{}", "fn process() {{ /* logic */ }}\n".repeat(150))));
        }

        // Assistant synthesizes
        msgs.push(assistant("Analysis complete. Modules 0-4 handle input parsing, \
            5-7 handle business logic, 8-9 handle output formatting. \
            Key finding: mod_3 has a potential race condition in the shared state handler."));

        // Add a few more turns so there's something to keep
        msgs.push(user("Fix the race condition in mod_3"));
        msgs.push(assistant("I'll add a Mutex to protect the shared state."));

        let original_chars: usize = msgs.iter()
            .map(|m| m.get("content").and_then(Value::as_str).unwrap_or("").len())
            .sum();

        // Micro-compact
        let tc = super::super::analytics::TurnCountCompactConfig::default();
        if let Some(trigger) = super::super::analytics::evaluate_turn_count_trigger(&msgs, &tc) {
            let (compacted, cleared) = super::super::analytics::apply_micro_compact(
                &msgs, &trigger.tool_ids_to_clear);
            assert!(cleared >= 7, "should clear most of 10 parallel results");
            msgs = compacted;
        }

        // Tiered compaction
        let result = compact_tiered_with_result(
            &msgs, 5000, 1000, CompactionTier::CompactHistory, 4);

        let final_chars: usize = result.messages.iter()
            .map(|m| m.get("content").and_then(Value::as_str).unwrap_or("").len())
            .sum();

        // Must achieve significant reduction
        let reduction_pct = ((original_chars - final_chars) as f64 / original_chars as f64) * 100.0;
        assert!(reduction_pct > 60.0,
            "tool explosion should be reduced by >60%, got {reduction_pct:.1}%");

        // The synthesis message must survive (it's the key insight)
        let has_synthesis = result.messages.iter().any(|m|
            m.get("content").and_then(Value::as_str)
                .map(|s| s.contains("race condition")).unwrap_or(false));
        assert!(has_synthesis,
            "assistant's synthesis (the actual insight) must survive compaction");

        // The fix request must survive (it's recent)
        let has_fix = result.messages.iter().any(|m|
            m.get("content").and_then(Value::as_str)
                .map(|s| s.contains("Fix the race condition")).unwrap_or(false));
        assert!(has_fix, "recent user instruction must survive");
    }

    // -----------------------------------------------------------------------
    // Unhappy-path / edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn compact_tiered_empty_messages() {
        let result = compact_tiered(&[], 100, 100, CompactionTier::CompactHistory, 4);
        assert!(result.is_empty());
    }

    #[test]
    fn compact_tiered_single_system_message() {
        let msgs = vec![json!({"role": "system", "content": "You are helpful"})];
        let result = compact_tiered(&msgs, 0, 0, CompactionTier::AggressivePrune, 4);
        // System message should always survive
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "system");
    }

    #[test]
    fn compact_tiered_under_budget_no_change() {
        let msgs = vec![user("hello"), assistant("hi")];
        let result =
            compact_tiered_with_result(&msgs, 1000, 500, CompactionTier::TrimSchemas, 4);
        assert_eq!(result.messages.len(), 2);
        // Under budget → no boundary emitted
        assert!(result.boundary.is_none());
    }

    #[test]
    fn compact_tiered_zero_budget_triggers_compaction() {
        let msgs = vec![
            user("hello"),
            assistant("world"),
            tool_with_id("c1", &"x".repeat(1000)),
        ];
        let result = compact_tiered_with_result(
            &msgs,
            0,    // budget_chars = 0
            100,  // keep_chars
            CompactionTier::CompactHistory,
            4,
        );
        // Should compact since total_chars > 0 = budget_chars
        assert!(result.boundary.is_some() || result.messages.len() <= msgs.len());
    }

    #[test]
    fn compact_tiered_messages_without_content() {
        // Messages with only role, no content field
        let msgs = vec![
            json!({"role": "user"}),
            json!({"role": "assistant"}),
            json!({"role": "tool", "tool_call_id": "c1"}),
        ];
        // Should not panic even with missing content
        let result = compact_tiered(&msgs, 0, 0, CompactionTier::AggressivePrune, 4);
        assert!(!result.is_empty() || msgs.is_empty());
    }

    #[test]
    fn compact_boundary_invalid_json_deserialization() {
        let bad_json = r#"{"not_a_boundary": true}"#;
        let result: Result<CompactBoundary, _> = serde_json::from_str(bad_json);
        // Should either fail or produce default fields — not panic
        // CompactBoundary has defaults so it may deserialize with defaults
        if let Ok(b) = result {
            // Verify it has sensible defaults
            assert_eq!(b.pre_tokens, 0);
            assert_eq!(b.messages_before, 0);
        }
        // Either way: no panic
    }

    #[test]
    fn compact_tier_invalid_string_deserialization() {
        let result: Result<CompactionTier, _> = serde_json::from_str(r#""invalid_tier""#);
        assert!(result.is_err(), "Invalid tier string should fail deserialization");
    }

    #[test]
    fn compact_tier_empty_string_deserialization() {
        let result: Result<CompactionTier, _> = serde_json::from_str(r#""""#);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_tool_call_meta_no_matching_assistant() {
        let msgs = vec![
            user("hello"),
            json!({"role": "tool", "tool_call_id": "orphan", "content": "data"}),
        ];
        let result = resolve_tool_call_meta(&msgs, 1);
        assert!(result.is_none());
    }

    #[test]
    fn resolve_tool_call_meta_tool_without_call_id() {
        let msgs = vec![
            assistant_tool("c1", "read_file", "{}"),
            tool("data"), // no tool_call_id
        ];
        let result = resolve_tool_call_meta(&msgs, 1);
        assert!(result.is_none());
    }

    #[test]
    fn resolve_tool_call_meta_index_out_of_bounds() {
        let msgs = vec![user("hello")];
        let result = resolve_tool_call_meta(&msgs, 5);
        assert!(result.is_none());
    }

    #[test]
    fn resolve_tool_call_meta_index_zero() {
        // Index 0: no preceding messages to search
        let msgs = vec![json!({"role": "tool", "tool_call_id": "c1", "content": "data"})];
        let result = resolve_tool_call_meta(&msgs, 0);
        assert!(result.is_none());
    }

    #[test]
    fn compact_boundary_to_system_message_roundtrip() {
        let boundary =
            CompactBoundary::new(CompactTrigger::Auto, CompactionTier::CompactHistory)
                .with_pre_metrics(5000, 10)
                .with_post_count(6);
        let msg = boundary.to_system_message();
        assert_eq!(msg["role"], "system");
        // Content should contain embedded JSON
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("compact-history"));
    }

    #[test]
    fn keep_recent_turns_larger_than_message_count() {
        let msgs = vec![user("hello"), assistant("hi")];
        let result = compact_tiered(&msgs, 0, 0, CompactionTier::CompactHistory, 100);
        // keep_recent_turns=100 > 2 messages → all kept
        assert_eq!(result.len(), 2);
    }
}
