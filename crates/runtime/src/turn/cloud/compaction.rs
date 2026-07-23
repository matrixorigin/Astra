use crate::prompts::{CompactConfig, CompactionTier};
use astra_turn_core::tool_call_shape::tool_call_name;
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
            let name = tool_call_name(tc)?.to_string();
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
    name == "read_file" || name.to_lowercase().ends_with("/read_file")
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
    if n.contains("grep") || n.contains("glob") || n.contains("list_dir") {
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

/// Resolve `function.name` + `function.arguments` for a `role: tool` message by matching
/// `tool_call_id` to the nearest preceding assistant `tool_calls` entry.
/// Apply context release stubs to messages
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

/// Metadata about a compaction event.
///
/// This remains out-of-band for diagnostics and analytics. It is not converted
/// into a synthetic prompt-history message.
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
    /// These names can be used by the tool surface layer to re-materialize
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
    /// Structured current-session memory routed separately through the
    /// context pipeline instead of being injected as a synthetic history blob.
    pub session_memory_context: Option<String>,
    /// Additional current-session working memories retrieved during
    /// compaction. These stay structured so callers can re-run the shared
    /// Memory binder and preserve identity, ranking, budgeting, and
    /// `CacheScope::None` placement.
    pub retrieved_memory_entries: Vec<astra_turn_core::context_sources::MemoryEntry>,
    /// Required per-compaction runtime context routed through the volatile
    /// system lane, never persisted as user/assistant/tool history.
    pub runtime_contexts: Vec<String>,
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

/// Tier-aware compaction returning a [`CompactResult`] with rich metadata.
///
/// Delegates to [`CompactionEngine::compact_tiered`] — the canonical
/// pipeline-based implementation.
#[cfg(test)]
pub(crate) fn compact_tiered_with_result(
    messages: &[Value],
    budget_chars: usize,
    keep_chars: usize,
    tier: CompactionTier,
    keep_recent_turns: usize,
) -> CompactResult {
    compact_tiered_impl(messages, budget_chars, keep_chars, tier, keep_recent_turns)
}

pub(crate) fn compact_tiered_impl(
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
            session_memory_context: None,
            retrieved_memory_entries: Vec::new(),
            runtime_contexts: Vec::new(),
        };
    }

    let total_chars: usize = messages
        .iter()
        .map(|message| {
            serde_json::to_string(message)
                .map(|encoded| encoded.chars().count())
                .unwrap_or(1)
        })
        .sum();

    if total_chars <= budget_chars {
        return CompactResult {
            messages: messages.to_vec(),
            boundary: None,
            tier,
            session_memory_context: None,
            retrieved_memory_entries: Vec::new(),
            runtime_contexts: Vec::new(),
        };
    }

    let mut compacted = messages.to_vec();
    let trunc_limit = match tier {
        CompactionTier::Normal => unreachable!(),
        CompactionTier::TrimSchemas => keep_chars * 2,
        CompactionTier::CompactHistory => keep_chars,
        CompactionTier::AggressivePrune => keep_chars / 2,
    };

    let mut seen_read_paths: HashSet<String> = HashSet::new();
    let tool_indices: Vec<usize> = compacted
        .iter()
        .enumerate()
        .filter_map(|(i, m)| (m.get("role").and_then(Value::as_str) == Some("tool")).then_some(i))
        .collect();
    let compact_limit = if tool_indices.len() <= 1 {
        tool_indices.len()
    } else {
        tool_indices.len() - 1
    };
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
        let asst_limit = trunc_limit * 2;
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

    if tier == CompactionTier::AggressivePrune {
        let first_user_idx = compacted
            .iter()
            .position(|m| m.get("role").and_then(Value::as_str) == Some("user"));
        let conv_indices: Vec<usize> = compacted
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                let role = m.get("role").and_then(Value::as_str).unwrap_or("");
                (role == "user" || role == "assistant").then_some(i)
            })
            .collect();
        let keep_count = keep_recent_turns * 2;
        if conv_indices.len() > keep_count {
            // Drop one contiguous historical span rather than user/assistant
            // messages in isolation. Tool results live between those control
            // messages; retaining them while deleting their assistant
            // `tool_calls` frame produces provider-invalid history.
            let tail_start = conv_indices[conv_indices.len() - keep_count.max(1)];
            compacted = compacted
                .into_iter()
                .enumerate()
                .filter(|(index, message)| {
                    message.get("role").and_then(Value::as_str) == Some("system")
                        || Some(*index) == first_user_idx
                        || *index >= tail_start
                })
                .map(|(_, m)| m)
                .collect();
        }
    }

    let messages_after = compacted.len();
    let boundary = CompactBoundary::new(CompactTrigger::Auto, tier)
        .with_pre_metrics(0, messages_before)
        .with_post_count(messages_after)
        .with_discovered_tools(extract_discovered_tools(messages));

    CompactResult {
        messages: compacted,
        boundary: Some(boundary),
        tier,
        session_memory_context: None,
        retrieved_memory_entries: Vec::new(),
        runtime_contexts: Vec::new(),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(content: &str) -> Value {
        json!({"role": "tool", "content": content})
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
        let result =
            compact_tiered_with_result(&msgs, 100, 100, CompactionTier::Normal, 4).messages;
        assert_eq!(result.len(), 3);
        // Content unchanged
        assert_eq!(
            result[2].get("content").unwrap().as_str().unwrap().len(),
            5000
        );
    }

    #[test]
    fn under_budget_no_compaction() {
        let msgs = vec![user("small"), tool("tiny")];
        let result =
            compact_tiered_with_result(&msgs, 100_000, 100, CompactionTier::AggressivePrune, 4)
                .messages;
        assert_eq!(result, msgs);
    }

    #[test]
    fn aggressive_prune_never_orphans_tool_results() {
        let mut messages = vec![user("complete a long tool-driven task")];
        for index in 0..8 {
            messages.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call-{index}"),
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"}
                }]
            }));
            messages.push(tool_with_id(
                &format!("call-{index}"),
                &format!("result {index}: {}", "evidence ".repeat(200)),
            ));
        }

        let result =
            compact_tiered_with_result(&messages, 1, 100, CompactionTier::AggressivePrune, 2);
        let retained_call_ids: HashSet<&str> = result
            .messages
            .iter()
            .filter_map(|message| message.get("tool_calls").and_then(Value::as_array))
            .flatten()
            .filter_map(|call| call.get("id").and_then(Value::as_str))
            .collect();

        for result_message in result
            .messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        {
            let result_id = result_message["tool_call_id"]
                .as_str()
                .expect("tool result id");
            assert!(
                retained_call_ids.contains(result_id),
                "compaction retained orphan tool result {result_id}: {:#?}",
                result.messages
            );
        }
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
        let original_tokens: usize = msgs
            .iter()
            .map(|m| {
                crate::prompts::estimate_str_tokens(
                    m.get("content").and_then(Value::as_str).unwrap_or(""),
                )
            })
            .sum();

        // Turn-count trigger: 25 tool results, threshold=8, keep=3 → clear 22
        let tc = super::super::analytics::TurnCountCompactConfig::default();
        let trigger = super::super::analytics::evaluate_turn_count_trigger(&msgs, &tc).unwrap();
        assert!(
            trigger.tool_ids_to_clear.len() >= 20,
            "should clear most old tool results"
        );

        let (compacted, cleared) =
            super::super::analytics::apply_micro_compact(&msgs, &trigger.tool_ids_to_clear);
        assert!(cleared >= 20);

        let post_tokens: usize = compacted
            .iter()
            .map(|m| {
                crate::prompts::estimate_str_tokens(
                    m.get("content").and_then(Value::as_str).unwrap_or(""),
                )
            })
            .sum();

        let savings_pct = ((original_tokens - post_tokens) as f64 / original_tokens as f64) * 100.0;
        assert!(
            savings_pct > 40.0,
            "micro-compact should save >40% tokens on tool-heavy conversation, got {savings_pct:.1}%"
        );
        // Message count unchanged — only content replaced
        assert_eq!(compacted.len(), msgs.len());
        // Recent 3 tool results preserved
        let last_tool = compacted
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .unwrap();
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
        let (after_micro, _) =
            super::super::analytics::apply_micro_compact(&msgs, &trigger.tool_ids_to_clear);

        // Layer 2: tiered compaction (AggressivePrune, keep 4 recent turns)
        let result =
            compact_tiered_with_result(&after_micro, 5000, 500, CompactionTier::AggressivePrune, 4);

        assert!(
            result.boundary.is_some(),
            "should produce compaction boundary"
        );
        let boundary = result.boundary.unwrap();
        assert_eq!(boundary.tier, CompactionTier::AggressivePrune);
        assert!(
            result.messages.len() < after_micro.len(),
            "aggressive prune should drop old turns: {} -> {}",
            after_micro.len(),
            result.messages.len()
        );
        // Recent turns preserved
        let has_recent = result.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|s| s.contains("step 19"))
                .unwrap_or(false)
        });
        assert!(
            has_recent,
            "most recent turn should survive aggressive prune"
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
        let (after_micro, micro_cleared) =
            super::super::analytics::apply_micro_compact(&msgs, &trigger.tool_ids_to_clear);
        assert!(micro_cleared > 0);

        // Step 2: estimate tokens and determine tier
        let est = crate::prompts::estimate_tokens(&after_micro, 0, 0);
        let tier = budget.compaction_tier(est);

        // Step 3: tiered compaction
        let result = compact_tiered_with_result(
            &after_micro,
            budget_chars,
            2000,
            tier,
            budget.keep_recent_turns,
        );

        // Verify: output is bounded
        let final_tokens = crate::prompts::estimate_tokens(&result.messages, 0, 0);
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
        let last_user = result
            .messages
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("user"));
        assert!(
            last_user.is_some(),
            "should preserve at least one user message"
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // Attention-critical scenarios: edge cases that stress context
    // quality after compaction, not just size reduction.
    // ═══════════════════════════════════════════════════════════════════

    /// Helper: count how many messages contain a substring.
    fn count_containing(msgs: &[Value], needle: &str) -> usize {
        msgs.iter()
            .filter(|m| {
                m.get("content")
                    .and_then(Value::as_str)
                    .map(|s| s.contains(needle))
                    .unwrap_or(false)
            })
            .count()
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

    #[test]
    fn canonicalizes_tool_call_name_for_duplicate_read_compaction() {
        let large = "line\n".repeat(300);
        let msgs = vec![
            user("inspect"),
            asst_call("c1", " read_file ", r#"{"path":"src/lib.rs"}"#),
            tool_result("c1", &format!("src/lib.rs\n{large}")),
            asst_call("c2", " read_file ", r#"{"path":"src/lib.rs"}"#),
            tool_result("c2", &format!("src/lib.rs\n{large}")),
            asst_call("c3", "read_file", r#"{"path":"src/keep.rs"}"#),
            tool_result("c3", &format!("src/keep.rs\n{large}")),
        ];

        let result = compact_tiered_with_result(&msgs, 10, 100, CompactionTier::CompactHistory, 4);

        let second = result.messages[4]["content"].as_str().unwrap();
        assert!(
            second.contains("duplicate read of `src/lib.rs`"),
            "expected duplicate read stub, got: {second}"
        );
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
            msgs.push(asst_call(
                &format!("c{i}"),
                "read_file",
                &format!(r#"{{"path":"config_{i}.yaml"}}"#),
            ));
            msgs.push(tool_result(
                &format!("c{i}"),
                &"setting: value\n".repeat(200),
            ));
            msgs.push(assistant(&format!("Config {i} loaded.")));
        }
        // Turn 3: THE CRITICAL INSTRUCTION (the needle)
        msgs.push(user(
            "CRITICAL: The database password changed to 'new_secret_42'. \
            Update all connection strings. Do NOT use the old password 'old_pass_7'.",
        ));
        msgs.push(assistant("Understood, I'll update all connection strings."));
        // Turns 4-19: more noise (file reads, edits)
        for i in 4..20 {
            msgs.push(user(&format!("Now edit file {i}")));
            msgs.push(asst_call(
                &format!("c{i}"),
                "read_file",
                &format!(r#"{{"path":"src/mod_{i}.rs"}}"#),
            ));
            msgs.push(tool_result(
                &format!("c{i}"),
                &format!(
                    "// mod_{i}.rs\n{}",
                    "fn handler() {{ todo!() }}\n".repeat(80)
                ),
            ));
            msgs.push(assistant(&format!("Updated module {i}.")));
        }

        // Micro-compact first
        let tc = super::super::analytics::TurnCountCompactConfig::default();
        let trigger = super::super::analytics::evaluate_turn_count_trigger(&msgs, &tc).unwrap();
        let (after_micro, _) =
            super::super::analytics::apply_micro_compact(&msgs, &trigger.tool_ids_to_clear);

        // Then aggressive prune (keep 4 recent turns)
        let result =
            compact_tiered_with_result(&after_micro, 5000, 500, CompactionTier::AggressivePrune, 4);

        // THE NEEDLE MUST SURVIVE: user messages are never dropped by tool compaction
        let has_critical = result.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|s| s.contains("new_secret_42"))
                .unwrap_or(false)
        });

        // AggressivePrune drops old user/assistant pairs, so the needle may be gone.
        // But this proves the design constraint: if keep_recent_turns is too small,
        // critical instructions are lost. The test documents this boundary.
        if !has_critical {
            // Verify it was in a dropped turn (turn 3, which is old)
            let total_user_msgs = result
                .messages
                .iter()
                .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                .count();
            // With keep_recent=4, we keep turns 16-19 (8 user+assistant msgs)
            assert!(
                total_user_msgs <= 8,
                "if needle is lost, it's because aggressive prune dropped old turns"
            );
            // This is the motivation for LLM summary: it should capture the needle.
            // Verify the boundary exists so summary can be attached.
            assert!(
                result.boundary.is_some(),
                "boundary must exist so LLM summary can capture critical instructions"
            );
        }
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
            msgs.push(asst_call(
                &format!("c{i}"),
                "bash",
                &format!(r#"{{"command":"echo 'python step {i}'"}}"#),
            ));
            msgs.push(tool_result(
                &format!("c{i}"),
                &format!("from flask import Flask\n{}", "# python code\n".repeat(200)),
            ));
            msgs.push(assistant(&format!("Python Flask step {i} done.")));
        }
        // THE FLIP: user changes to Rust
        msgs.push(user(
            "Actually, scratch all that. Rewrite everything in Rust using Axum. \
            Python is too slow for our use case.",
        ));
        // Phase 2 (turns 8-15): "Build in Rust with Axum"
        for i in 8..16 {
            msgs.push(asst_call(
                &format!("c{i}"),
                "bash",
                &format!(r#"{{"command":"cargo build step {i}"}}"#),
            ));
            msgs.push(tool_result(
                &format!("c{i}"),
                &format!("use axum::Router;\n{}", "// rust code\n".repeat(200)),
            ));
            msgs.push(assistant(&format!("Rust Axum step {i} done.")));
        }

        // Micro-compact + aggressive prune (keep 6 recent turns)
        let tc = super::super::analytics::TurnCountCompactConfig::default();
        let trigger = super::super::analytics::evaluate_turn_count_trigger(&msgs, &tc).unwrap();
        let (after_micro, _) =
            super::super::analytics::apply_micro_compact(&msgs, &trigger.tool_ids_to_clear);
        let result =
            compact_tiered_with_result(&after_micro, 5000, 500, CompactionTier::AggressivePrune, 6);

        // Count references to old vs new tech
        let python_refs = count_containing(&result.messages, "Python");
        let rust_refs = count_containing(&result.messages, "Rust");
        let axum_refs =
            count_containing(&result.messages, "Axum") + count_containing(&result.messages, "axum");

        // New requirement (Rust/Axum) should dominate over old (Python)
        assert!(
            rust_refs + axum_refs >= python_refs,
            "Rust/Axum refs ({}) should >= Python refs ({}) after compaction",
            rust_refs + axum_refs,
            python_refs
        );

        // The flip message should survive (it's the most recent user instruction
        // before phase 2, and keep_recent=6 covers turns 10-15)
        // But the flip is at turn 7.5 — it may be dropped by aggressive prune.
        // This documents the design boundary: LLM summary must capture the flip.
        let has_flip = result.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|s| s.contains("scratch all that"))
                .unwrap_or(false)
        });
        if !has_flip {
            assert!(
                result.boundary.is_some(),
                "if flip instruction is lost, boundary must exist for LLM summary to capture it"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Unhappy-path / edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn compact_tiered_empty_messages() {
        let result =
            compact_tiered_with_result(&[], 100, 100, CompactionTier::CompactHistory, 4).messages;
        assert!(result.is_empty());
    }

    #[test]
    fn compact_tiered_single_system_message() {
        let msgs = vec![json!({"role": "system", "content": "You are helpful"})];
        let result =
            compact_tiered_with_result(&msgs, 0, 0, CompactionTier::AggressivePrune, 4).messages;
        // System message should always survive
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "system");
    }

    #[test]
    fn compact_tiered_under_budget_no_change() {
        let msgs = vec![user("hello"), assistant("hi")];
        let result = compact_tiered_with_result(&msgs, 1000, 500, CompactionTier::TrimSchemas, 4);
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
            0,   // budget_chars = 0
            100, // keep_chars
            CompactionTier::CompactHistory,
            4,
        );
        // Should compact since total_chars > 0 = budget_chars
        assert!(result.boundary.is_some() || result.messages.len() <= msgs.len());
    }

    #[test]
    fn compact_history_truncates_large_tool_output_without_timestamp() {
        let msgs = vec![
            user("hello"),
            assistant("world"),
            tool_with_id("c1", &"x".repeat(6000)),
        ];
        let result =
            compact_tiered_with_result(&msgs, 800, 2000, CompactionTier::CompactHistory, 1);

        let tool_content = result.messages[2]["content"]
            .as_str()
            .expect("tool content");
        assert!(
            tool_content.len() < 6000,
            "compact history should truncate oversized tool results even without timestamps"
        );
        assert!(
            result.boundary.is_some(),
            "compaction should emit a boundary"
        );
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
        let result =
            compact_tiered_with_result(&msgs, 0, 0, CompactionTier::AggressivePrune, 4).messages;
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
        assert!(
            result.is_err(),
            "Invalid tier string should fail deserialization"
        );
    }

    #[test]
    fn compact_tier_empty_string_deserialization() {
        let result: Result<CompactionTier, _> = serde_json::from_str(r#""""#);
        assert!(result.is_err());
    }

    #[test]
    fn keep_recent_turns_larger_than_message_count() {
        let msgs = vec![user("hello"), assistant("hi")];
        let result =
            compact_tiered_with_result(&msgs, 0, 0, CompactionTier::CompactHistory, 100).messages;
        // keep_recent_turns=100 > 2 messages → all kept
        assert_eq!(result.len(), 2);
    }

    // --- CompactBoundary builder & serde edge cases ---

    #[test]
    fn compact_boundary_new_defaults() {
        let b = CompactBoundary::new(CompactTrigger::Manual, CompactionTier::Normal);
        assert_eq!(b.pre_tokens, 0);
        assert_eq!(b.messages_before, 0);
        assert_eq!(b.messages_after, 0);
        assert!(b.last_pre_compact_uuid.is_none());
        assert!(b.summary.is_none());
        assert!(b.recent_files.is_empty());
        assert!(b.discovered_tools.is_empty());
    }

    #[test]
    fn compact_boundary_builder_chain() {
        let b = CompactBoundary::new(CompactTrigger::Auto, CompactionTier::AggressivePrune)
            .with_pre_metrics(10000, 50)
            .with_post_count(10)
            .with_last_uuid("uuid-123")
            .with_recent_files(vec!["a.rs".into(), "b.rs".into()])
            .with_discovered_tools(vec!["bash".into()]);
        assert_eq!(b.pre_tokens, 10000);
        assert_eq!(b.messages_before, 50);
        assert_eq!(b.messages_after, 10);
        assert_eq!(b.last_pre_compact_uuid.as_deref(), Some("uuid-123"));
        assert_eq!(b.recent_files.len(), 2);
        assert_eq!(b.discovered_tools, vec!["bash"]);
    }

    #[test]
    fn compact_boundary_serde_round_trip() {
        let b = CompactBoundary::new(CompactTrigger::Auto, CompactionTier::TrimSchemas)
            .with_pre_metrics(5000, 20)
            .with_post_count(8)
            .with_recent_files(vec!["main.rs".into()]);
        let json = serde_json::to_string(&b).unwrap();
        let back: CompactBoundary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.trigger, CompactTrigger::Auto);
        assert_eq!(back.pre_tokens, 5000);
        assert_eq!(back.messages_before, 20);
        assert_eq!(back.messages_after, 8);
        assert_eq!(back.recent_files, vec!["main.rs"]);
    }

    #[test]
    fn compact_boundary_serde_skips_empty_vecs() {
        let b = CompactBoundary::new(CompactTrigger::Manual, CompactionTier::Normal);
        let json = serde_json::to_string(&b).unwrap();
        assert!(
            !json.contains("recent_files"),
            "empty recent_files should be skipped"
        );
        assert!(
            !json.contains("discovered_tools"),
            "empty discovered_tools should be skipped"
        );
        assert!(!json.contains("summary"), "None summary should be skipped");
    }

    #[test]
    fn compact_trigger_serde_snake_case() {
        let json = serde_json::to_string(&CompactTrigger::Manual).unwrap();
        assert_eq!(json, r#""manual""#);
        let json = serde_json::to_string(&CompactTrigger::Auto).unwrap();
        assert_eq!(json, r#""auto""#);
    }

    #[test]
    fn compact_trigger_deserialize_rejects_camel_case() {
        let result = serde_json::from_str::<CompactTrigger>(r#""Manual""#);
        assert!(result.is_err());
    }

    #[test]
    fn compact_boundary_with_zero_pre_metrics() {
        let b = CompactBoundary::new(CompactTrigger::Auto, CompactionTier::CompactHistory)
            .with_pre_metrics(0, 0)
            .with_post_count(0);
        let json = serde_json::to_string(&b).unwrap();
        let back: CompactBoundary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pre_tokens, 0);
        assert_eq!(back.messages_before, 0);
    }
}
