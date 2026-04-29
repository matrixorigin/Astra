//! Microcompact: clear old tool result content before each LLM call.
//!
//! Tool results (file reads, grep output, git diffs) dominate history token cost. Once the LLM has acted
//! on a tool result, the full content is rarely needed again. This module
//! replaces old tool result content with a short placeholder, keeping only the
//! most recent N results intact.
//!
//! Two triggers (whichever fires first):
//! - **Count-based**: more than `KEEP_RECENT` compactable results → clear oldest
//! - **Token-based**: total compactable content exceeds `TOKEN_BUDGET` → clear
//!   oldest until under budget (even if count ≤ KEEP_RECENT)
//!
//! This runs in-place on `state.messages` before each `execute_turn` call.

use serde_json::Value;

/// Placeholder that replaces cleared tool result content.
pub const CLEARED_PLACEHOLDER: &str = "[Previous tool output cleared]";

/// Maximum length for the normalized args preview in the placeholder.
const ARGS_PREVIEW_MAX: usize = 120;

/// Provider-aware compaction strategy.
///
/// Controls how cleared tool results are replaced:
/// - `Normalized`: stable `key=value` placeholder (OpenAI/GLM/DeepSeek — prefix caching)
/// - `Minimal`: shortest possible placeholder (Anthropic — protocol-level caching)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompactStrategy {
    /// `[Cleared: tool_name key=value — re-run if needed]`
    /// Stable, normalized args (no raw JSON). Good for prefix-caching providers.
    #[default]
    Normalized,
    /// `[Cleared]`
    /// Minimal placeholder. Anthropic uses cache_control at the protocol layer,
    /// so placeholder content doesn't affect cache hits.
    Minimal,
}

/// Provider prompt-cache protocol class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptCacheProtocol {
    /// Provider benefits from stable deterministic prompt prefixes.
    #[default]
    Prefix,
    /// Provider supports Anthropic-style `cache_control`, `cache_reference`,
    /// and `cache_edits` request metadata.
    AnthropicCacheControl,
}

/// Explicit cache/compaction capabilities derived from the selected provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCacheStrategy {
    pub prompt_cache_protocol: PromptCacheProtocol,
    pub compact_strategy: CompactStrategy,
    pub supports_cache_control: bool,
    /// Capability flag consumed by API-layer request annotation code.
    pub supports_cache_reference: bool,
    /// Capability flag consumed by API-layer request annotation code.
    pub supports_cache_edits: bool,
}

impl Default for ProviderCacheStrategy {
    fn default() -> Self {
        Self {
            prompt_cache_protocol: PromptCacheProtocol::Prefix,
            compact_strategy: CompactStrategy::Normalized,
            supports_cache_control: false,
            supports_cache_reference: false,
            supports_cache_edits: false,
        }
    }
}

impl ProviderCacheStrategy {
    /// Derive provider cache capabilities from a provider or model hint.
    ///
    /// This is intentionally capability-shaped rather than placeholder-shaped:
    /// OpenAI-compatible providers keep stable local placeholders for prefix
    /// caching, while Anthropic-compatible providers prefer protocol-level
    /// cache metadata and minimal local mutation.
    pub fn from_provider_hint(provider_or_model: &str) -> Self {
        let lower = provider_or_model.to_ascii_lowercase();
        if lower.contains("claude") || lower.contains("anthropic") {
            Self {
                prompt_cache_protocol: PromptCacheProtocol::AnthropicCacheControl,
                compact_strategy: CompactStrategy::Minimal,
                supports_cache_control: true,
                supports_cache_reference: true,
                supports_cache_edits: true,
            }
        } else {
            Self::default()
        }
    }

    /// Derive provider cache capabilities with an explicit provider taking
    /// precedence over model name. This avoids misclassifying OpenAI-compatible
    /// proxies that serve Claude-named models.
    pub fn from_provider_and_model(provider: Option<&str>, model: Option<&str>) -> Self {
        if let Some(provider) = provider.filter(|value| !value.trim().is_empty()) {
            let from_provider = Self::from_provider_hint(provider);
            // If the provider is explicitly Anthropic, trust it.
            if from_provider.prompt_cache_protocol == PromptCacheProtocol::AnthropicCacheControl {
                return from_provider;
            }
            // If the provider is a known non-Anthropic API (OpenAI, Gemini, etc.),
            // respect that even when the model name contains "claude" — the caller
            // is explicitly routing through a non-Anthropic endpoint.
            // Unknown providers (e.g. openrouter, litellm) fall through to model
            // detection so that Claude models served via proxy get the right protocol.
            let lower = provider.to_ascii_lowercase();
            let is_known_non_anthropic = lower.contains("openai")
                || lower.contains("gemini")
                || lower.contains("google")
                || lower.contains("mistral")
                || lower.contains("cohere")
                || lower.contains("groq")
                || lower.contains("together")
                || lower.contains("deepseek")
                || lower.contains("qwen")
                || lower.contains("ollama");
            if is_known_non_anthropic {
                return from_provider;
            }
        }
        model.map(Self::from_provider_hint).unwrap_or_default()
    }
}

impl CompactStrategy {
    /// Derive strategy from provider/model name.
    /// Anthropic (claude) → Minimal; everything else → Normalized.
    pub fn from_provider_hint(provider_or_model: &str) -> Self {
        ProviderCacheStrategy::from_provider_hint(provider_or_model).compact_strategy
    }

    /// Derive strategy from explicit provider plus model fallback.
    pub fn from_provider_and_model(provider: Option<&str>, model: Option<&str>) -> Self {
        ProviderCacheStrategy::from_provider_and_model(provider, model).compact_strategy
    }
}

/// Returns `true` if content looks like a cleared placeholder (any variant).
pub fn is_cleared_content(content: &str) -> bool {
    content == CLEARED_PLACEHOLDER || content == "[Cleared]" || content.starts_with("[Cleared: ")
}

/// Stable arg keys worth preserving in the normalized placeholder.
/// These identify *what* was accessed, not *how* (command content is volatile).
const STABLE_ARG_KEYS: &[&str] = &[
    "path",
    "file_path",
    "pattern",
    "symbol_name",
    "symbol",
    "url",
    "query",
    "glob",
    "ref",
    "commit",
];

/// Extract normalized `key=value` pairs from JSON args string.
/// Only keeps stable keys, truncates long values.
fn normalize_args(raw: &str) -> String {
    let Ok(obj) = serde_json::from_str::<serde_json::Map<String, Value>>(raw) else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    let mut len = 0usize;
    for key in STABLE_ARG_KEYS {
        if let Some(val) = obj.get(*key) {
            let v = match val {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            // Truncate long values (e.g. long file paths are fine, but cap at 80)
            let v = if v.len() > 80 {
                format!("{}…", &v[..80])
            } else {
                v
            };
            let part = format!("{key}={v}");
            len += part.len() + 1;
            if len > ARGS_PREVIEW_MAX {
                break;
            }
            parts.push(part);
        }
    }
    parts.join(" ")
}

/// Tool call metadata extracted from assistant messages (owned strings to avoid borrow issues).
struct ToolCallMaps {
    id_to_name: std::collections::HashMap<String, String>,
    id_to_args: std::collections::HashMap<String, String>,
}

/// Build owned tool_call_id → (name, args) maps from assistant messages.
fn build_tool_call_maps(messages: &[Value]) -> ToolCallMaps {
    let mut id_to_name = std::collections::HashMap::new();
    let mut id_to_args = std::collections::HashMap::new();
    for msg in messages.iter() {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for tc in calls {
            if let (Some(id), Some(name)) = (
                tc.get("id").and_then(Value::as_str),
                tc.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str),
            ) {
                id_to_name.insert(id.to_string(), name.to_string());
                if let Some(args) = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                {
                    id_to_args.insert(id.to_string(), args.to_string());
                }
            }
        }
    }
    ToolCallMaps {
        id_to_name,
        id_to_args,
    }
}

impl ToolCallMaps {
    /// Build the cleared placeholder for a given tool_call_id, respecting strategy.
    fn cleared_placeholder(&self, call_id: &str, strategy: CompactStrategy) -> String {
        match strategy {
            CompactStrategy::Minimal => "[Cleared]".to_string(),
            CompactStrategy::Normalized => {
                let Some(name) = self.id_to_name.get(call_id) else {
                    return CLEARED_PLACEHOLDER.to_string();
                };
                let preview = self
                    .id_to_args
                    .get(call_id)
                    .map(|args| normalize_args(args))
                    .unwrap_or_default();
                if preview.is_empty() {
                    format!("[Cleared: {name} — re-run if needed]")
                } else {
                    format!("[Cleared: {name} {preview} — re-run if needed]")
                }
            }
        }
    }

    /// Borrow id_to_name as &str refs for is_compactable_tool_result.
    fn name_ref_map(&self) -> std::collections::HashMap<&str, &str> {
        self.id_to_name
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
}

/// Marker for tool results persisted to disk by `tool_result_storage`.
/// These contain a file reference the LLM needs to re-read the output.
const PERSISTED_TAG: &str = "<persisted-output>";

/// Tool names whose results are safe to compact (read-only, reproducible).
/// Excluded: bash (non-idempotent), write_file/str_replace (mutation records),
/// skill (instructions), delegate (delegation records).
const COMPACTABLE_TOOLS: &[&str] = &[
    "read_file",
    "grep",
    "glob",
    "list_dir",
    "git_show",
    "git_diff",
    "git_log",
    "git_status",
    "git_blame",
    "git_file_history",
    "git_contributors",
    "git_log_search",
    "web_search",
    "web_fetch",
    // Code intel tools (idempotent reads, can produce large output)
    "symbols",
    "find_definition",
    "find_references",
    "symbol_search",
    "hover_info",
    "call_graph",
    "type_hierarchy",
    "dead_code",
    "extract_members",
    // GitHub read-only tools
    "github_list_prs",
    "github_get_pr",
    "github_ci_status",
    "github_list_issues",
    "github_get_issue",
    "github_repo_stats",
    "get_agent_info",
];

/// How many recent compactable tool results to keep intact.
const KEEP_RECENT: usize = 6;

/// Maximum total estimated tokens for compactable tool results.
/// When exceeded, clear oldest results even if count ≤ KEEP_RECENT.
/// 12K tokens ≈ 48KB of content — enough for ~6 medium file reads.
const TOKEN_BUDGET: usize = 12_000;

/// Minimum content length (bytes) to bother compacting.
/// Short results cost few tokens and provide useful context.
const MIN_COMPACT_SIZE: usize = 500;

/// Pressure-adaptive compaction parameters.
///
/// When context pressure rises, keep fewer results and use a tighter token
/// budget so that the next LLM call has more headroom.
#[derive(Debug, Clone, Copy)]
pub struct AdaptiveCompactConfig {
    pub keep_recent: usize,
    pub token_budget: usize,
}

impl Default for AdaptiveCompactConfig {
    fn default() -> Self {
        Self {
            keep_recent: KEEP_RECENT,
            token_budget: TOKEN_BUDGET,
        }
    }
}

impl AdaptiveCompactConfig {
    /// Compute adaptive parameters from context pressure (0.0–1.0+).
    ///
    /// | Pressure      | keep_recent | token_budget |
    /// |---------------|-------------|--------------|
    /// | < 0.60        | 6           | 12 000       |
    /// | 0.60 – 0.75   | 4           | 8 000        |
    /// | 0.75 – 0.90   | 2           | 4 000        |
    /// | ≥ 0.90        | 1           | 2 000        |
    pub fn from_pressure(pressure: f64) -> Self {
        if pressure >= 0.90 {
            Self {
                keep_recent: 1,
                token_budget: 2_000,
            }
        } else if pressure >= 0.75 {
            Self {
                keep_recent: 2,
                token_budget: 4_000,
            }
        } else if pressure >= 0.60 {
            Self {
                keep_recent: 4,
                token_budget: 8_000,
            }
        } else {
            Self::default()
        }
    }
}

/// Rough token estimate for a string. ~4 bytes per token for English/code.
/// Underestimates for CJK (~2 bytes/token) — acceptable since the budget
/// is a soft threshold, not a hard limit.
fn estimate_tokens(s: &str) -> usize {
    s.len() / 4
}

/// Compact old tool results in the message history.
///
/// Returns the number of tool results compacted and estimated tokens saved.
pub fn compact_tool_results(
    messages: &mut [Value],
    keep_recent: Option<usize>,
    strategy: CompactStrategy,
) -> CompactStats {
    crate::chat_history_openai::sanitize_empty_assistant_tool_calls_mut(messages);
    let config = keep_recent
        .map(|k| AdaptiveCompactConfig {
            keep_recent: k,
            token_budget: TOKEN_BUDGET,
        })
        .unwrap_or_default();
    compact_tool_results_with_config(messages, &config, strategy)
}

/// Pressure-adaptive variant: compact with parameters derived from context
/// pressure so that high-pressure turns free more headroom.
pub fn compact_tool_results_adaptive(
    messages: &mut [Value],
    pressure: f64,
    strategy: CompactStrategy,
) -> CompactStats {
    crate::chat_history_openai::sanitize_empty_assistant_tool_calls_mut(messages);
    let config = AdaptiveCompactConfig::from_pressure(pressure);
    compact_tool_results_with_config(messages, &config, strategy)
}

/// Pressure-adaptive compaction with optional disk persistence.
///
/// When `session_dir` is `Some`, full tool result content is persisted to disk
/// via `tool_result_storage` before being cleared. This allows session resume
/// to recover the full content from `~/.astra/sessions/<id>/tool-results/`.
///
/// When `session_dir` is `None`, behaves identically to `compact_tool_results_adaptive`.
pub fn compact_tool_results_adaptive_with_persistence(
    messages: &mut [Value],
    pressure: f64,
    strategy: CompactStrategy,
    session_dir: Option<&std::path::Path>,
) -> CompactStats {
    crate::chat_history_openai::sanitize_empty_assistant_tool_calls_mut(messages);
    let config = AdaptiveCompactConfig::from_pressure(pressure);
    compact_tool_results_with_persistence(messages, &config, strategy, session_dir)
}

/// State-aware variant: uses `SessionFacts.active_files` as a pin list.
/// Files actively being worked on (last `pin_turns` turns) are never compacted,
/// regardless of count/token pressure. Other files follow normal compaction rules.
pub fn compact_tool_results_state_aware(
    messages: &mut [Value],
    pressure: f64,
    facts: &crate::cloud_session_facts::SessionFacts,
    pin_turns: u32,
    strategy: CompactStrategy,
) -> CompactStats {
    crate::chat_history_openai::sanitize_empty_assistant_tool_calls_mut(messages);
    let config = AdaptiveCompactConfig::from_pressure(pressure);
    compact_tool_results_with_pin_list(messages, &config, facts, pin_turns, strategy, None)
}

/// State-aware compaction with optional disk persistence.
pub fn compact_tool_results_state_aware_with_persistence(
    messages: &mut [Value],
    pressure: f64,
    facts: &crate::cloud_session_facts::SessionFacts,
    pin_turns: u32,
    strategy: CompactStrategy,
    session_dir: Option<&std::path::Path>,
) -> CompactStats {
    crate::chat_history_openai::sanitize_empty_assistant_tool_calls_mut(messages);
    let config = AdaptiveCompactConfig::from_pressure(pressure);
    compact_tool_results_with_pin_list(messages, &config, facts, pin_turns, strategy, session_dir)
}

fn compact_tool_results_with_pin_list(
    messages: &mut [Value],
    config: &AdaptiveCompactConfig,
    facts: &crate::cloud_session_facts::SessionFacts,
    pin_turns: u32,
    strategy: CompactStrategy,
    session_dir: Option<&std::path::Path>,
) -> CompactStats {
    let keep = config.keep_recent;

    let maps = build_tool_call_maps(messages);
    let id_to_name = maps.name_ref_map();

    // Collect compactable results, split into pinned vs unpinned
    let mut unpinned: Vec<(usize, usize)> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if !is_compactable_tool_result(msg, &id_to_name) {
            continue;
        }
        let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
        if content.len() < MIN_COMPACT_SIZE || is_cleared_content(content) {
            continue;
        }
        // Check if this result's file is in the active pin list
        let file_path = extract_file_path_from_tool_result(msg, &id_to_name);
        let is_pinned = file_path
            .as_deref()
            .map(|p| facts.is_active_file(p, pin_turns) || facts.is_pending_relevant_file(p))
            .unwrap_or(false);
        if !is_pinned {
            unpinned.push((i, estimate_tokens(content)));
        }
        // Pinned files are never compacted
    }

    if unpinned.is_empty() {
        return CompactStats::default();
    }

    // Apply normal compaction rules only to unpinned results
    let count_based = unpinned.len().saturating_sub(keep);
    let total_tokens: usize = unpinned.iter().map(|(_, t)| t).sum();
    let budget = config.token_budget;
    let token_based = if total_tokens > budget {
        let mut cumulative = 0usize;
        let mut n = 0usize;
        for &(_, tokens) in &unpinned {
            if total_tokens - cumulative <= budget {
                break;
            }
            cumulative += tokens;
            n += 1;
        }
        n.min(unpinned.len() - 1)
    } else {
        0
    };

    let to_compact = count_based.max(token_based);
    let mut stats = CompactStats::default();

    for &(idx, tokens) in unpinned.iter().take(to_compact) {
        let call_id = messages[idx]
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        // If a session_dir is configured, we MUST successfully persist to disk
        // before clearing. A failed write followed by a clear would silently
        // lose the tool output. If persistence fails, skip this entry so the
        // content survives in-memory for the next compaction attempt.
        if let Some(dir) = session_dir {
            if let Some(content) = messages[idx].get("content").and_then(Value::as_str) {
                let content = content.to_string();
                let tool_name = id_to_name
                    .get(call_id.as_str())
                    .copied()
                    .or_else(|| messages[idx].get("name").and_then(Value::as_str))
                    .unwrap_or("unknown")
                    .to_string();
                let persisted = crate::tool_result_storage::maybe_persist_tool_result_unconditional(
                    dir, &call_id, &tool_name, &content,
                );
                if !persisted {
                    // Disk write failed — do not clear; keep the content in memory.
                    continue;
                }
            }
        }

        stats.tokens_saved += tokens;
        stats.results_compacted += 1;
        messages[idx]["content"] = Value::String(maps.cleared_placeholder(&call_id, strategy));
    }

    stats
}

/// Extract file path from a tool result message (for pin list matching).
fn extract_file_path_from_tool_result(
    msg: &Value,
    id_to_name: &std::collections::HashMap<&str, &str>,
) -> Option<String> {
    // For read_file results, the content often starts with the file path
    let tool_name = msg.get("name").and_then(Value::as_str).or_else(|| {
        msg.get("tool_call_id")
            .and_then(Value::as_str)
            .and_then(|id| id_to_name.get(id).copied())
    })?;
    if !matches!(
        tool_name,
        "read_file" | "grep" | "glob" | "git_show" | "git_diff"
    ) {
        return None;
    }
    // Try to extract path from content (read_file results typically start with path)
    let content = msg.get("content").and_then(Value::as_str)?;
    // Pattern 1: read_file output with tab-separated format "1\t/path/to/file"
    let first_line = content.lines().next().unwrap_or("");
    // Try to find a path-like segment (contains /, no spaces, reasonable length)
    for segment in first_line.split(|c: char| c.is_whitespace() || c == '\t') {
        // Skip line numbers (pure digits)
        if segment.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        // Valid path: contains /, no spaces (already split), reasonable length
        if segment.contains('/') && segment.len() < 200 && segment.len() > 1 {
            // Skip if it looks like a content fragment (starts with non-path chars)
            if segment.starts_with('/') || segment.starts_with("./") || segment.starts_with("../") {
                return Some(segment.to_string());
            }
            // Also accept relative paths (e.g. "src/main.rs") — must have a file extension
            // to avoid false positives like "v1.2/feature" or prose fragments.
            if segment
                .chars()
                .next()
                .map_or(false, |c| c.is_alphanumeric())
                && segment
                    .rsplit('/')
                    .next()
                    .map_or(false, |f| f.contains('.'))
            {
                return Some(segment.to_string());
            }
        }
    }
    None
}

fn compact_tool_results_with_config(
    messages: &mut [Value],
    config: &AdaptiveCompactConfig,
    strategy: CompactStrategy,
) -> CompactStats {
    let keep = config.keep_recent;

    let maps = build_tool_call_maps(messages);
    let id_to_name = maps.name_ref_map();

    // Collect (index, content_tokens) of compactable tool result messages.
    let compactable: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, msg)| {
            if !is_compactable_tool_result(msg, &id_to_name) {
                return None;
            }
            let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
            if content.len() < MIN_COMPACT_SIZE || is_cleared_content(content) {
                return None;
            }
            Some((i, estimate_tokens(content)))
        })
        .collect();

    if compactable.is_empty() {
        return CompactStats::default();
    }

    // Determine how many to compact: max of count-based and token-based.
    let count_based = compactable.len().saturating_sub(keep);

    // Token-based: find the minimum number of oldest results to clear
    // so that the remaining total stays under the configured token budget.
    let total_tokens: usize = compactable.iter().map(|(_, t)| t).sum();
    let budget = config.token_budget;
    let token_based = if total_tokens > budget {
        let mut cumulative = 0usize;
        let mut n = 0usize;
        for &(_, tokens) in &compactable {
            if total_tokens - cumulative <= budget {
                break;
            }
            cumulative += tokens;
            n += 1;
        }
        // Always keep at least 1 result
        n.min(compactable.len() - 1)
    } else {
        0
    };

    let to_compact = count_based.max(token_based);
    let mut stats = CompactStats::default();

    for &(idx, tokens) in compactable.iter().take(to_compact) {
        stats.tokens_saved += tokens;
        stats.results_compacted += 1;
        let call_id = messages[idx]
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        messages[idx]["content"] = Value::String(maps.cleared_placeholder(call_id, strategy));
    }

    stats
}

fn compact_tool_results_with_persistence(
    messages: &mut [Value],
    config: &AdaptiveCompactConfig,
    strategy: CompactStrategy,
    session_dir: Option<&std::path::Path>,
) -> CompactStats {
    crate::chat_history_openai::sanitize_empty_assistant_tool_calls_mut(messages);
    let keep = config.keep_recent;

    let maps = build_tool_call_maps(messages);
    let id_to_name = maps.name_ref_map();

    let compactable: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, msg)| {
            if !is_compactable_tool_result(msg, &id_to_name) {
                return None;
            }
            let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
            if content.len() < MIN_COMPACT_SIZE || is_cleared_content(content) {
                return None;
            }
            Some((i, estimate_tokens(content)))
        })
        .collect();

    if compactable.is_empty() {
        return CompactStats::default();
    }

    let count_based = compactable.len().saturating_sub(keep);
    let total_tokens: usize = compactable.iter().map(|(_, t)| t).sum();
    let budget = config.token_budget;
    let token_based = if total_tokens > budget {
        let mut cumulative = 0usize;
        let mut n = 0usize;
        for &(_, tokens) in &compactable {
            if total_tokens - cumulative <= budget {
                break;
            }
            cumulative += tokens;
            n += 1;
        }
        n.min(compactable.len() - 1)
    } else {
        0
    };

    let to_compact = count_based.max(token_based);
    let mut stats = CompactStats::default();

    for &(idx, tokens) in compactable.iter().take(to_compact) {
        let call_id = messages[idx]
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        // Persist full content to disk before clearing. If persistence fails,
        // skip this entry — clearing without a successful write would lose data.
        if let Some(dir) = session_dir {
            if let Some(content) = messages[idx].get("content").and_then(Value::as_str) {
                let content = content.to_string();
                let tool_name = id_to_name
                    .get(call_id.as_str())
                    .copied()
                    .or_else(|| messages[idx].get("name").and_then(Value::as_str))
                    .unwrap_or("unknown")
                    .to_string();
                let persisted = crate::tool_result_storage::maybe_persist_tool_result_unconditional(
                    dir, &call_id, &tool_name, &content,
                );
                if !persisted {
                    continue;
                }
            }
        }

        stats.tokens_saved += tokens;
        stats.results_compacted += 1;
        messages[idx]["content"] = Value::String(maps.cleared_placeholder(&call_id, strategy));
    }

    stats
}

/// Check if a message is a tool result from a compactable tool.
fn is_compactable_tool_result(
    msg: &Value,
    id_to_name: &std::collections::HashMap<&str, &str>,
) -> bool {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
    if role != "tool" {
        return false;
    }
    // Skip persisted-to-disk results — they contain a file reference
    // the LLM needs to re-read the output.
    if let Some(content) = msg.get("content").and_then(Value::as_str) {
        if content.contains(PERSISTED_TAG) {
            return false;
        }
    }
    // Check tool name from the message itself
    if let Some(name) = msg.get("name").and_then(Value::as_str) {
        return COMPACTABLE_TOOLS.contains(&name);
    }
    // Look up tool name via tool_call_id → assistant message mapping
    if let Some(call_id) = msg.get("tool_call_id").and_then(Value::as_str) {
        if let Some(&name) = id_to_name.get(call_id) {
            return COMPACTABLE_TOOLS.contains(&name);
        }
    }
    // Unknown tool — don't compact (could be bash, skill, or write_file)
    false
}

#[derive(Debug, Default)]
pub struct CompactStats {
    pub results_compacted: usize,
    pub tokens_saved: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant_with_tools(calls: &[(&str, &str)]) -> Value {
        let tool_calls: Vec<Value> = calls
            .iter()
            .map(|(id, name)| json!({"id": id, "function": {"name": name}}))
            .collect();
        json!({"role": "assistant", "content": "", "tool_calls": tool_calls})
    }

    fn tool_result(id: &str, content: &str) -> Value {
        json!({"role": "tool", "tool_call_id": id, "content": content})
    }

    /// Check if a message's content is a cleared placeholder (old or enhanced).
    fn content_is_cleared(msg: &Value) -> bool {
        msg.get("content")
            .and_then(Value::as_str)
            .map(super::is_cleared_content)
            .unwrap_or(false)
    }

    #[test]
    fn estimate_tokens_reasonable_for_code() {
        // Typical code: ~4 bytes/token. 1000 bytes → ~250 tokens.
        assert_eq!(estimate_tokens(&"x".repeat(1000)), 250);
        assert_eq!(estimate_tokens(&"x".repeat(4)), 1);
        assert_eq!(estimate_tokens(""), 0);
        // Short content rounds down
        assert_eq!(estimate_tokens("abc"), 0);
    }

    // ── Count-based compaction ───────────────────────────────────────────

    #[test]
    fn compacts_old_results_keeps_recent() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            json!({"role": "user", "content": "review code"}),
            assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file"), ("c3", "grep")]),
            tool_result("c1", &big),
            tool_result("c2", &big),
            tool_result("c3", &big),
            assistant_with_tools(&[("c4", "read_file"), ("c5", "read_file")]),
            tool_result("c4", &big),
            tool_result("c5", &big),
        ];

        let stats = compact_tool_results(&mut messages, Some(2), Default::default());

        assert_eq!(stats.results_compacted, 3);
        assert!(
            content_is_cleared(&messages[2]),
            "expected cleared at [2], got: {:?}",
            messages[2]["content"]
        );
        assert!(
            content_is_cleared(&messages[3]),
            "expected cleared at [3], got: {:?}",
            messages[3]["content"]
        );
        assert!(
            content_is_cleared(&messages[4]),
            "expected cleared at [4], got: {:?}",
            messages[4]["content"]
        );
        assert_eq!(messages[6]["content"], big); // recent kept
        assert_eq!(messages[7]["content"], big);
    }

    // ── Token-based compaction ───────────────────────────────────────────

    #[test]
    fn token_budget_triggers_even_under_keep_count() {
        // 3 results, each ~5K tokens = 15K total > TOKEN_BUDGET (12K).
        // keep=6, so count-based wouldn't trigger. Token-based should.
        let huge = "x".repeat(20_000); // ~5K tokens
        let mut messages = vec![
            assistant_with_tools(&[
                ("c1", "read_file"),
                ("c2", "read_file"),
                ("c3", "read_file"),
            ]),
            tool_result("c1", &huge),
            tool_result("c2", &huge),
            tool_result("c3", &huge),
        ];

        let stats = compact_tool_results(&mut messages, Some(6), Default::default());

        // Should compact at least 1 to get under budget
        assert!(
            stats.results_compacted >= 1,
            "token budget should trigger compaction even with count < keep, got {}",
            stats.results_compacted
        );
        // c3 (most recent) should be preserved
        assert!(
            !content_is_cleared(&messages[3]),
            "expected NOT cleared at [3]"
        );
    }

    #[test]
    fn token_budget_always_keeps_at_least_one() {
        // 1 giant result that exceeds budget alone — token-based path
        // should still keep it (can't compact the only result).
        let giant = "x".repeat(100_000); // ~25K tokens >> TOKEN_BUDGET
        let mut messages = vec![
            assistant_with_tools(&[("c1", "read_file")]),
            tool_result("c1", &giant),
        ];

        // With keep=6 (default), count-based won't trigger (1 < 6).
        // Token-based wants to clear, but min(n, len-1) = min(1, 0) = 0.
        let stats = compact_tool_results(&mut messages, None, Default::default());

        assert_eq!(stats.results_compacted, 0);
        assert!(
            !content_is_cleared(&messages[1]),
            "expected NOT cleared at [1]"
        );
    }

    // ── Safety: non-compactable tools ────────────────────────────────────

    #[test]
    fn skips_bash_skill_write_file() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            assistant_with_tools(&[
                ("c1", "bash"),
                ("c2", "skill"),
                ("c3", "write_file"),
                ("c4", "str_replace"),
                ("c5", "delegate"),
                ("c6", "read_file"),
            ]),
            tool_result("c1", &big),
            tool_result("c2", &big),
            tool_result("c3", &big),
            tool_result("c4", &big),
            tool_result("c5", &big),
            tool_result("c6", &big),
        ];

        let stats = compact_tool_results(&mut messages, Some(0), Default::default());

        assert_eq!(stats.results_compacted, 1); // only read_file
        assert_eq!(messages[1]["content"], big); // bash
        assert_eq!(messages[2]["content"], big); // skill
        assert_eq!(messages[3]["content"], big); // write_file
        assert_eq!(messages[4]["content"], big); // str_replace
        assert_eq!(messages[5]["content"], big); // delegate
        assert!(
            content_is_cleared(&messages[6]),
            "expected cleared at [6], got: {:?}",
            messages[6]["content"]
        ); // read_file
    }

    #[test]
    fn skips_unknown_tool_call_ids() {
        let big = "x".repeat(1000);
        let mut messages = vec![tool_result("orphan", &big)];

        let stats = compact_tool_results(&mut messages, Some(0), Default::default());
        assert_eq!(stats.results_compacted, 0);
        assert_eq!(messages[0]["content"], big);
    }

    // ── Safety: persisted-to-disk results ────────────────────────────────

    #[test]
    fn skips_persisted_to_disk_results() {
        let persisted = "<persisted-output>\nTool `read_file` produced 50000 chars.\n\
             File: /tmp/sessions/tool_results/c1.txt\n\
             Preview: first 500 chars...\n</persisted-output>"
            .to_string();
        let mut messages = vec![
            assistant_with_tools(&[("c1", "read_file")]),
            tool_result("c1", &persisted),
        ];

        let stats = compact_tool_results(&mut messages, Some(0), Default::default());

        assert_eq!(stats.results_compacted, 0);
        assert!(
            messages[1]["content"]
                .as_str()
                .unwrap()
                .contains("<persisted-output>")
        );
    }

    // ── Edge cases ───────────────────────────────────────────────────────

    #[test]
    fn skips_short_results() {
        let mut messages = vec![
            assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file")]),
            tool_result("c1", "short"),
            tool_result("c2", "also short"),
        ];

        let stats = compact_tool_results(&mut messages, Some(0), Default::default());
        assert_eq!(stats.results_compacted, 0);
    }

    #[test]
    fn skips_non_tool_messages() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            json!({"role": "user", "content": &big}),
            json!({"role": "assistant", "content": &big}),
        ];

        let stats = compact_tool_results(&mut messages, Some(0), Default::default());
        assert_eq!(stats.results_compacted, 0);
        assert_eq!(messages[0]["content"], big);
        assert_eq!(messages[1]["content"], big);
    }

    #[test]
    fn idempotent_on_already_compacted() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file")]),
            tool_result("c1", &big),
            tool_result("c2", &big),
        ];

        compact_tool_results(&mut messages, Some(0), Default::default());
        assert!(
            content_is_cleared(&messages[1]),
            "expected cleared at [1], got: {:?}",
            messages[1]["content"]
        );

        let stats = compact_tool_results(&mut messages, Some(0), Default::default());
        assert_eq!(stats.results_compacted, 0); // already cleared
    }

    #[test]
    fn no_compaction_when_under_both_thresholds() {
        let small = "x".repeat(600); // > MIN_COMPACT_SIZE but small tokens
        let mut messages = vec![
            assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file")]),
            tool_result("c1", &small),
            tool_result("c2", &small),
        ];

        let stats = compact_tool_results(&mut messages, Some(5), Default::default());
        assert_eq!(stats.results_compacted, 0); // 2 results < keep=5, tokens < budget
    }

    // ── Realistic scenario: review session ───────────────────────────────

    #[test]
    fn realistic_review_session_compaction() {
        // Simulate session 746b6423: skill + 15 file reads across 2 iterations.
        // Iteration 1: skill(review-changes) + 11 read_file + 4 grep
        // Iteration 2: 3 more read_file
        // Before iteration 2, microcompact should clear old results.

        let file_content = "fn main() {\n".repeat(80); // ~960 bytes, realistic file
        let grep_output = "src/main.rs:10: fn main()\nsrc/lib.rs:5: pub fn run()\n".repeat(10);
        let skill_output = "# Review\nLooks good.\n".repeat(50); // ~1050 bytes

        let mut messages = vec![
            json!({"role": "user", "content": "review latest commit"}),
            // Iteration 1: assistant calls skill + tools
            assistant_with_tools(&[
                ("s1", "skill"),
                ("r1", "read_file"),
                ("r2", "read_file"),
                ("r3", "read_file"),
                ("r4", "read_file"),
                ("r5", "read_file"),
                ("r6", "read_file"),
                ("r7", "read_file"),
                ("r8", "read_file"),
                ("r9", "read_file"),
                ("r10", "read_file"),
                ("r11", "read_file"),
                ("g1", "grep"),
                ("g2", "grep"),
                ("g3", "grep"),
                ("g4", "grep"),
            ]),
            tool_result("s1", &skill_output),
            tool_result("r1", &file_content),
            tool_result("r2", &file_content),
            tool_result("r3", &file_content),
            tool_result("r4", &file_content),
            tool_result("r5", &file_content),
            tool_result("r6", &file_content),
            tool_result("r7", &file_content),
            tool_result("r8", &file_content),
            tool_result("r9", &file_content),
            tool_result("r10", &file_content),
            tool_result("r11", &file_content),
            tool_result("g1", &grep_output),
            tool_result("g2", &grep_output),
            tool_result("g3", &grep_output),
            tool_result("g4", &grep_output),
        ];

        // Before iteration 2: run microcompact
        let stats = compact_tool_results(&mut messages, None, Default::default()); // default keep=6

        // 15 compactable (11 read_file + 4 grep), skill is NOT compactable.
        // Count-based: 15 - 6 = 9 to compact.
        // Token-based: 15 * ~240 tokens = ~3600 tokens < 12K budget → no extra.
        assert_eq!(stats.results_compacted, 9);

        // Skill output preserved (not compactable)
        assert!(
            !content_is_cleared(&messages[2]),
            "expected NOT cleared at [2]"
        );
        assert!(messages[2]["content"].as_str().unwrap().contains("Review"));

        // Most recent 6 compactable results preserved
        // (g1, g2, g3, g4 are indices 15-18, r10, r11 are indices 12-13)
        // The last 6 in order: r6..r11? No — compactable order is r1..r11, g1..g4
        // Last 6: r11, g1, g2, g3, g4 + one more = r10
        // Actually: compactable indices are [3..13, 14..17] = r1..r11, g1..g4
        // Last 6: indices for r10, r11, g1, g2, g3, g4

        // Verify oldest are cleared
        assert!(
            content_is_cleared(&messages[3]),
            "expected cleared at [3], got: {:?}",
            messages[3]["content"]
        ); // r1
        assert!(
            content_is_cleared(&messages[4]),
            "expected cleared at [4], got: {:?}",
            messages[4]["content"]
        ); // r2

        // Verify newest are kept
        assert!(
            !content_is_cleared(&messages[17]),
            "expected NOT cleared at [17]"
        ); // g4
        assert!(
            !content_is_cleared(&messages[16]),
            "expected NOT cleared at [16]"
        ); // g3

        // Token savings: 9 results * ~240 tokens each ≈ 2160
        assert!(
            stats.tokens_saved > 1500,
            "expected meaningful savings, got {}",
            stats.tokens_saved
        );
    }

    #[test]
    fn realistic_large_file_reads_trigger_token_budget() {
        // Scenario: 4 large file reads (each ~4K tokens = 16K bytes).
        // Count < keep=6, but total tokens (16K) > TOKEN_BUDGET (12K).
        let large_file = "x".repeat(16_000); // ~4K tokens each

        let mut messages = vec![
            assistant_with_tools(&[
                ("r1", "read_file"),
                ("r2", "read_file"),
                ("r3", "read_file"),
                ("r4", "read_file"),
            ]),
            tool_result("r1", &large_file),
            tool_result("r2", &large_file),
            tool_result("r3", &large_file),
            tool_result("r4", &large_file),
        ];

        let stats = compact_tool_results(&mut messages, None, Default::default()); // keep=6

        // Count-based: 4 < 6 → 0. But token-based: 4*4K = 16K > 12K → must clear some.
        assert!(
            stats.results_compacted >= 1,
            "token budget should trigger on large files, got {} compacted",
            stats.results_compacted
        );

        // Most recent should be preserved
        assert!(
            !content_is_cleared(&messages[4]),
            "expected NOT cleared at [4]"
        ); // r4 (newest)

        // Total remaining tokens should be under budget
        let remaining_tokens: usize = messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .filter(|m| {
                m.get("content")
                    .and_then(Value::as_str)
                    .map(|c| !super::is_cleared_content(c))
                    .unwrap_or(true)
            })
            .map(|m| estimate_tokens(m.get("content").and_then(Value::as_str).unwrap_or("")))
            .sum();
        assert!(
            remaining_tokens <= TOKEN_BUDGET,
            "remaining tokens {} should be <= budget {}",
            remaining_tokens,
            TOKEN_BUDGET
        );
    }

    #[test]
    fn mixed_compactable_and_non_compactable_interleaved() {
        // Real pattern: read_file → bash(make test) → read_file → grep
        // bash results must survive even when surrounded by compactable tools.
        let big = "x".repeat(1000);
        let test_output = "test result: ok. 42 passed; 0 failed".repeat(20); // ~720 bytes

        let mut messages = vec![
            assistant_with_tools(&[
                ("r1", "read_file"),
                ("b1", "bash"),
                ("r2", "read_file"),
                ("g1", "grep"),
            ]),
            tool_result("r1", &big),
            tool_result("b1", &test_output),
            tool_result("r2", &big),
            tool_result("g1", &big),
        ];

        let stats = compact_tool_results(&mut messages, Some(1), Default::default());

        // 3 compactable (r1, r2, g1), keep 1 → compact 2 (r1, r2)
        assert_eq!(stats.results_compacted, 2);
        assert!(
            content_is_cleared(&messages[1]),
            "expected cleared at [1], got: {:?}",
            messages[1]["content"]
        ); // r1 compacted
        assert!(
            messages[2]["content"]
                .as_str()
                .unwrap()
                .contains("test result")
        ); // bash preserved!
        assert!(
            content_is_cleared(&messages[3]),
            "expected cleared at [3], got: {:?}",
            messages[3]["content"]
        ); // r2 compacted
        assert_eq!(messages[4]["content"], big); // g1 kept (most recent compactable)
    }

    // ── Goal preservation after compaction ────────────────────────────

    #[test]
    fn goal_context_preserved_after_multi_round_compaction() {
        // Simulate a real multi-round session:
        // Round 1: user asks to fix a bug. LLM reads 8 files, runs tests.
        // Round 2: LLM reads 4 more files, makes a fix.
        // Round 3: LLM runs tests again.
        // After compaction before round 3, verify:
        // - User's original request survives
        // - LLM's analysis/conclusions survive (assistant text)
        // - Test output (bash) survives
        // - File paths in tool_calls survive (LLM knows what to re-read)
        // - Cleared results have placeholder (not deleted)

        let file_content = "fn buggy() { panic!(); }\n".repeat(40); // ~1KB
        let test_fail = "FAILED: test_foo - assertion failed at line 42\nExpected: 5\nGot: 3";
        let test_pass = "test result: ok. 10 passed; 0 failed";

        let mut messages = vec![
            // User goal
            json!({"role": "user", "content": "Fix the bug in src/parser.rs that causes test_foo to fail"}),
            // Round 1: LLM reads files and runs tests
            json!({"role": "assistant", "content": "I'll investigate the test failure. Let me read the relevant files and run the tests.", "tool_calls": [
                {"id": "r1", "function": {"name": "read_file", "arguments": "{\"path\": \"src/parser.rs\"}"}},
                {"id": "r2", "function": {"name": "read_file", "arguments": "{\"path\": \"src/lexer.rs\"}"}},
                {"id": "r3", "function": {"name": "read_file", "arguments": "{\"path\": \"src/ast.rs\"}"}},
                {"id": "r4", "function": {"name": "read_file", "arguments": "{\"path\": \"tests/test_parser.rs\"}"}},
                {"id": "r5", "function": {"name": "read_file", "arguments": "{\"path\": \"src/lib.rs\"}"}},
                {"id": "r6", "function": {"name": "read_file", "arguments": "{\"path\": \"src/error.rs\"}"}},
                {"id": "r7", "function": {"name": "read_file", "arguments": "{\"path\": \"src/token.rs\"}"}},
                {"id": "r8", "function": {"name": "read_file", "arguments": "{\"path\": \"Cargo.toml\"}"}},
                {"id": "b1", "function": {"name": "bash", "arguments": "{\"command\": \"cargo test test_foo\"}"}},
            ]}),
            tool_result("r1", &file_content),
            tool_result("r2", &file_content),
            tool_result("r3", &file_content),
            tool_result("r4", &file_content),
            tool_result("r5", &file_content),
            tool_result("r6", &file_content),
            tool_result("r7", &file_content),
            tool_result("r8", &file_content),
            tool_result("b1", test_fail),
            // Round 1 conclusion
            json!({"role": "assistant", "content": "I found the bug. In src/parser.rs line 42, the parse_expr function returns the wrong precedence value (3 instead of 5). The fix is to change the constant on line 42."}),
            // Round 2: LLM reads more files and applies fix
            json!({"role": "assistant", "content": "Let me apply the fix.", "tool_calls": [
                {"id": "r9", "function": {"name": "read_file", "arguments": "{\"path\": \"src/parser.rs:40-50\"}"}},
                {"id": "r10", "function": {"name": "read_file", "arguments": "{\"path\": \"src/precedence.rs\"}"}},
                {"id": "r11", "function": {"name": "read_file", "arguments": "{\"path\": \"src/constants.rs\"}"}},
                {"id": "r12", "function": {"name": "read_file", "arguments": "{\"path\": \"tests/test_precedence.rs\"}"}},
                {"id": "w1", "function": {"name": "str_replace", "arguments": "{\"path\": \"src/parser.rs\"}"}},
            ]}),
            tool_result("r9", &file_content),
            tool_result("r10", &file_content),
            tool_result("r11", &file_content),
            tool_result("r12", &file_content),
            tool_result("w1", "Applied: replaced '3' with '5' on line 42"),
            // Round 2 conclusion
            json!({"role": "assistant", "content": "Fix applied. Now let me run the tests to verify.", "tool_calls": [
                {"id": "b2", "function": {"name": "bash", "arguments": "{\"command\": \"cargo test\"}"}},
            ]}),
            tool_result("b2", test_pass),
        ];

        // Run compaction (simulating what happens before round 3)
        let stats = compact_tool_results(&mut messages, None, Default::default());

        // Should compact some old read_file results
        assert!(stats.results_compacted > 0, "should compact old file reads");

        // ── GOAL PRESERVATION CHECKS ──

        // 1. User's original request is intact
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("Fix the bug in src/parser.rs"),
            "user's goal must survive compaction"
        );

        // 2. LLM's analysis/conclusions are intact (assistant text)
        let assistant_texts: Vec<&str> = messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect();
        assert!(
            assistant_texts
                .iter()
                .any(|t| t.contains("parse_expr function returns the wrong precedence")),
            "LLM's bug analysis must survive"
        );
        assert!(
            assistant_texts.iter().any(|t| t.contains("Fix applied")),
            "LLM's fix confirmation must survive"
        );

        // 3. Test outputs (bash) survive — these are critical evidence
        let bash_results: Vec<&str> = messages
            .iter()
            .filter(|m| {
                m.get("role").and_then(Value::as_str) == Some("tool")
                    && (m.get("tool_call_id").and_then(Value::as_str) == Some("b1")
                        || m.get("tool_call_id").and_then(Value::as_str) == Some("b2"))
            })
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect();
        assert!(
            bash_results.iter().any(|t| t.contains("FAILED")),
            "original test failure output must survive"
        );
        assert!(
            bash_results.iter().any(|t| t.contains("ok. 10 passed")),
            "test pass output must survive"
        );

        // 4. str_replace result survives (mutation record)
        let w1_content = messages
            .iter()
            .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("w1"))
            .and_then(|m| m.get("content").and_then(Value::as_str))
            .unwrap();
        assert!(
            w1_content.contains("Applied"),
            "write/edit result must survive: got '{}'",
            w1_content
        );

        // 5. File paths in tool_calls survive (LLM can re-read if needed)
        let all_tool_calls: Vec<&str> = messages
            .iter()
            .filter_map(|m| m.get("tool_calls").and_then(Value::as_array))
            .flat_map(|calls| calls.iter())
            .filter_map(|tc| {
                tc.get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
            })
            .collect();
        assert!(
            all_tool_calls.iter().any(|a| a.contains("src/parser.rs")),
            "file paths in tool_calls must survive for re-reading"
        );

        // 6. Cleared results have placeholder, not deleted
        let cleared: Vec<&Value> = messages
            .iter()
            .filter(|m| {
                m.get("content")
                    .and_then(Value::as_str)
                    .map(super::is_cleared_content)
                    .unwrap_or(false)
            })
            .collect();
        assert!(!cleared.is_empty(), "some results should be cleared");
        for msg in &cleared {
            assert!(
                msg.get("tool_call_id").is_some(),
                "cleared results must retain tool_call_id for context"
            );
        }

        // 7. Total message count unchanged (no messages deleted)
        assert_eq!(
            messages.len(),
            20,
            "no messages should be deleted, only content replaced"
        );
    }

    // ── Complex / edge-case tests ────────────────────────────────────

    #[test]
    fn progressive_compaction_across_multiple_rounds() {
        // Simulate: compact after round 2, add more tools, compact again after round 3.
        // Verifies compaction compounds correctly and doesn't double-clear.
        let big = "x".repeat(800);

        let mut messages = vec![
            json!({"role": "user", "content": "task"}),
            // Round 1: 4 reads
            assistant_with_tools(&[
                ("c1", "read_file"),
                ("c2", "read_file"),
                ("c3", "grep"),
                ("c4", "read_file"),
            ]),
            tool_result("c1", &big),
            tool_result("c2", &big),
            tool_result("c3", &big),
            tool_result("c4", &big),
        ];

        // Compact after round 1 — 4 compactable, under keep=6, no compaction
        let s1 = compact_tool_results(&mut messages, None, Default::default());
        assert_eq!(s1.results_compacted, 0);

        // Round 2: 4 more reads (total 8 compactable > keep=6)
        messages.push(assistant_with_tools(&[
            ("c5", "read_file"),
            ("c6", "grep"),
            ("c7", "git_diff"),
            ("c8", "read_file"),
        ]));
        messages.extend([
            tool_result("c5", &big),
            tool_result("c6", &big),
            tool_result("c7", &big),
            tool_result("c8", &big),
        ]);

        // Compact after round 2 — 8 compactable, clear oldest 2
        let s2 = compact_tool_results(&mut messages, None, Default::default());
        assert_eq!(s2.results_compacted, 2);
        assert!(
            content_is_cleared(&messages[2]),
            "expected cleared at [2], got: {:?}",
            messages[2]["content"]
        ); // c1
        assert!(
            content_is_cleared(&messages[3]),
            "expected cleared at [3], got: {:?}",
            messages[3]["content"]
        ); // c2
        assert!(
            !content_is_cleared(&messages[4]),
            "expected NOT cleared at [4]"
        ); // c3 kept

        // Round 3: 3 more reads (total 9 non-cleared compactable > keep=6)
        messages.push(assistant_with_tools(&[
            ("c9", "read_file"),
            ("c10", "grep"),
            ("c11", "read_file"),
        ]));
        messages.extend([
            tool_result("c9", &big),
            tool_result("c10", &big),
            tool_result("c11", &big),
        ]);

        // Compact after round 3 — should clear more old ones, NOT re-clear c1/c2
        let s3 = compact_tool_results(&mut messages, None, Default::default());
        assert!(s3.results_compacted > 0, "should compact more old results");
        // c1, c2 already cleared — should still be placeholder (idempotent)
        assert!(
            content_is_cleared(&messages[2]),
            "expected cleared at [2], got: {:?}",
            messages[2]["content"]
        );
        assert!(
            content_is_cleared(&messages[3]),
            "expected cleared at [3], got: {:?}",
            messages[3]["content"]
        );
        // Total non-cleared compactable should be <= KEEP_RECENT
        let live = messages
            .iter()
            .filter(|m| {
                m.get("role").and_then(Value::as_str) == Some("tool")
                    && m.get("content")
                        .and_then(Value::as_str)
                        .map(|c| !super::is_cleared_content(c))
                        .unwrap_or(true)
                    && m.get("content")
                        .and_then(Value::as_str)
                        .map_or(false, |c| c.len() >= MIN_COMPACT_SIZE)
            })
            .count();
        assert!(
            live <= KEEP_RECENT,
            "at most {} live compactable results, got {}",
            KEEP_RECENT,
            live
        );
    }

    #[test]
    fn token_budget_boundary_exact() {
        // Exactly at TOKEN_BUDGET — should NOT trigger token-based compaction.
        // 4 results × 3000 tokens each = 12000 = TOKEN_BUDGET exactly.
        // The trigger condition is `>` (strict), so exactly-at-budget is safe.
        let content = "x".repeat(12_000); // ~3000 tokens
        let mut messages = vec![
            json!({"role": "user", "content": "task"}),
            assistant_with_tools(&[
                ("c1", "read_file"),
                ("c2", "read_file"),
                ("c3", "grep"),
                ("c4", "git_diff"),
            ]),
            tool_result("c1", &content),
            tool_result("c2", &content),
            tool_result("c3", &content),
            tool_result("c4", &content),
        ];

        // 4 compactable < keep=6, so count-based won't trigger.
        // Token-based: 4 × 3000 = 12000 = TOKEN_BUDGET. Condition is >, not >=.
        let stats = compact_tool_results(&mut messages, None, Default::default());
        assert_eq!(
            stats.results_compacted, 0,
            "exactly at budget should not trigger (> not >=)"
        );
        for m in &messages[2..6] {
            assert!(!content_is_cleared(m), "expected NOT cleared");
        }
    }

    #[test]
    fn mixed_tool_calls_in_single_assistant_message() {
        // One assistant message calls read_file + bash + write_file.
        // Only read_file should be compactable.
        let big = "x".repeat(800);
        let mut messages = vec![
            json!({"role": "user", "content": "task"}),
            // 3 assistant messages, each with mixed tools, to exceed keep=6
            assistant_with_tools(&[("c1", "read_file"), ("c2", "bash"), ("c3", "write_file")]),
            tool_result("c1", &big),
            tool_result("c2", &big),
            tool_result("c3", &big),
            assistant_with_tools(&[("c4", "read_file"), ("c5", "bash"), ("c6", "str_replace")]),
            tool_result("c4", &big),
            tool_result("c5", &big),
            tool_result("c6", &big),
            assistant_with_tools(&[("c7", "read_file"), ("c8", "bash"), ("c9", "read_file")]),
            tool_result("c7", &big),
            tool_result("c8", &big),
            tool_result("c9", &big),
            // 4th round to push read_file count past keep
            assistant_with_tools(&[
                ("c10", "read_file"),
                ("c11", "grep"),
                ("c12", "read_file"),
                ("c13", "read_file"),
                ("c14", "read_file"),
            ]),
            tool_result("c10", &big),
            tool_result("c11", &big),
            tool_result("c12", &big),
            tool_result("c13", &big),
            tool_result("c14", &big),
        ];

        let stats = compact_tool_results(&mut messages, None, Default::default());

        // bash results must NEVER be compacted
        for id in ["c2", "c5", "c8"] {
            let m = messages
                .iter()
                .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some(id))
                .unwrap();
            assert_eq!(
                m["content"].as_str().unwrap(),
                &big,
                "bash result {} must survive",
                id
            );
        }
        // write_file / str_replace must NEVER be compacted
        for id in ["c3", "c6"] {
            let m = messages
                .iter()
                .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some(id))
                .unwrap();
            assert_eq!(
                m["content"].as_str().unwrap(),
                &big,
                "mutation result {} must survive",
                id
            );
        }
        // Some read_file/grep should be compacted
        assert!(
            stats.results_compacted > 0,
            "should compact some read-only results"
        );
    }

    #[test]
    fn cache_stub_not_re_compacted() {
        // A cache stub (~90 bytes) is under MIN_COMPACT_SIZE.
        // Verify it's not touched by microcompact.
        let stub = "(cached — identical call already executed in this conversation. \
                     Re-read the file only if you need the content again.)";
        let big = "x".repeat(800);

        let mut messages = vec![
            json!({"role": "user", "content": "task"}),
            // 8 results: 1 stub + 7 big reads (to exceed keep=6)
            assistant_with_tools(&[
                ("c1", "read_file"),
                ("c2", "read_file"),
                ("c3", "read_file"),
                ("c4", "read_file"),
                ("c5", "read_file"),
                ("c6", "read_file"),
                ("c7", "read_file"),
                ("c8", "read_file"),
            ]),
            tool_result("c1", stub), // cache stub — small, should be skipped
            tool_result("c2", &big),
            tool_result("c3", &big),
            tool_result("c4", &big),
            tool_result("c5", &big),
            tool_result("c6", &big),
            tool_result("c7", &big),
            tool_result("c8", &big),
        ];

        compact_tool_results(&mut messages, None, Default::default());

        // Stub must survive untouched
        assert_eq!(
            messages[2]["content"].as_str().unwrap(),
            stub,
            "cache stub must not be compacted (under MIN_COMPACT_SIZE)"
        );
    }

    #[test]
    fn persisted_output_mixed_with_compactable_in_same_turn() {
        // One assistant turn produces both a persisted-output result and
        // a normal compactable result. Only the normal one should compact.
        let persisted =
            "<persisted-output>Preview of large file... (saved to /tmp/abc)</persisted-output>";
        let big = "x".repeat(800);

        // Need >6 compactable (non-persisted) to trigger count-based.
        // 10 total: 2 persisted + 8 normal compactable → 8 > keep=6 → clear 2.
        let mut messages = vec![
            json!({"role": "user", "content": "task"}),
            assistant_with_tools(&[
                ("c1", "read_file"),
                ("c2", "read_file"),
                ("c3", "read_file"),
                ("c4", "read_file"),
                ("c5", "read_file"),
                ("c6", "read_file"),
                ("c7", "read_file"),
                ("c8", "read_file"),
                ("c9", "read_file"),
                ("c10", "read_file"),
            ]),
            tool_result("c1", persisted), // persisted — must survive
            tool_result("c2", &big),
            tool_result("c3", &big),
            tool_result("c4", &big),
            tool_result("c5", persisted), // persisted — must survive
            tool_result("c6", &big),
            tool_result("c7", &big),
            tool_result("c8", &big),
            tool_result("c9", &big),
            tool_result("c10", &big),
        ];

        let stats = compact_tool_results(&mut messages, None, Default::default());
        assert!(stats.results_compacted > 0);

        // Both persisted results must survive
        assert_eq!(
            messages[2]["content"].as_str().unwrap(),
            persisted,
            "c1 persisted must survive"
        );
        assert_eq!(
            messages[6]["content"].as_str().unwrap(),
            persisted,
            "c5 persisted must survive"
        );
    }

    #[test]
    fn stress_50_tools_across_15_iterations() {
        // Stress test: 50+ tool results across 15 iterations.
        // Verifies no panic, correct bounds, and reasonable compaction.
        let big = "x".repeat(1000);
        let mut messages: Vec<Value> = vec![json!({"role": "user", "content": "big task"})];

        let tools_per_iter = [4, 5, 3, 4, 3, 4, 3, 3, 4, 3, 3, 2, 3, 3, 2];
        let mut call_id = 0u32;
        let mut all_tool_names: Vec<(String, String)> = Vec::new(); // (id, name)

        for (iter, &count) in tools_per_iter.iter().enumerate() {
            let tool_calls: Vec<(&str, String)> = (0..count)
                .map(|j| {
                    call_id += 1;
                    let name = match j % 4 {
                        0 => "read_file",
                        1 => "grep",
                        2 => {
                            if iter % 3 == 0 {
                                "bash"
                            } else {
                                "git_diff"
                            }
                        }
                        _ => "glob",
                    };
                    (name, format!("s{}", call_id))
                })
                .collect();

            let tc_pairs: Vec<(&str, &str)> =
                tool_calls.iter().map(|(n, id)| (id.as_str(), *n)).collect();
            messages.push(assistant_with_tools(&tc_pairs));

            for (name, id) in &tool_calls {
                let content = if *name == "bash" { "ok" } else { &big };
                messages.push(tool_result(id, content));
                all_tool_names.push((id.clone(), name.to_string()));
            }

            // Run microcompact before each iteration (except first)
            if iter > 0 {
                compact_tool_results(&mut messages, None, Default::default());
            }
        }

        // Final compaction
        compact_tool_results(&mut messages, None, Default::default());

        // Structural integrity: every tool result has tool_call_id and content
        let tool_msgs: Vec<&Value> = messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .collect();
        for m in &tool_msgs {
            assert!(
                m.get("tool_call_id").is_some(),
                "every tool result must have tool_call_id"
            );
            assert!(
                m.get("content").is_some(),
                "every tool result must have content"
            );
        }

        // bash results must all survive (non-compactable)
        for (id, name) in &all_tool_names {
            if name == "bash" {
                let m = messages
                    .iter()
                    .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some(id.as_str()))
                    .unwrap();
                assert!(!content_is_cleared(m), "bash {} must survive", id);
            }
        }

        // Total tool results count unchanged (no deletions)
        let total_tool_count: usize = tools_per_iter.iter().sum();
        assert_eq!(
            tool_msgs.len(),
            total_tool_count,
            "no tool messages deleted: expected {}, got {}",
            total_tool_count,
            tool_msgs.len()
        );

        // Some compaction must have happened
        let cleared_count = tool_msgs
            .iter()
            .filter(|m| {
                m.get("content")
                    .and_then(Value::as_str)
                    .map(super::is_cleared_content)
                    .unwrap_or(false)
            })
            .count();
        assert!(cleared_count > 0, "stress test should trigger compaction");
    }

    #[test]
    fn non_string_content_not_compacted() {
        // OpenAI vision format: content can be an array. Must not crash or compact.
        let mut messages = vec![
            json!({"role": "user", "content": "task"}),
            assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file")]),
            json!({"role": "tool", "tool_call_id": "c1", "content": [
                {"type": "text", "text": "file content here that is long enough to exceed min compact size threshold for testing purposes"}
            ]}),
            tool_result("c2", &"x".repeat(800)),
        ];

        // Should not panic on array content
        let stats = compact_tool_results(&mut messages, None, Default::default());
        // Array content treated as size 0 → skipped
        assert!(
            messages[2]["content"].is_array(),
            "array content must be preserved as-is"
        );
        assert_eq!(
            stats.results_compacted, 0,
            "nothing to compact (1 array + 1 under keep)"
        );
    }

    #[test]
    fn empty_and_null_content_handled() {
        let mut messages = vec![
            json!({"role": "user", "content": "task"}),
            assistant_with_tools(&[
                ("c1", "read_file"),
                ("c2", "read_file"),
                ("c3", "read_file"),
            ]),
            json!({"role": "tool", "tool_call_id": "c1", "content": ""}),
            json!({"role": "tool", "tool_call_id": "c2", "content": null}),
            tool_result("c3", &"x".repeat(800)),
        ];

        // Should not panic
        let stats = compact_tool_results(&mut messages, None, Default::default());
        assert_eq!(stats.results_compacted, 0, "empty/null/single under keep");
        assert_eq!(messages[2]["content"], "");
        assert!(messages[3]["content"].is_null());
    }

    // ── Adaptive compaction ───────────────────────────────────────────────

    #[test]
    fn adaptive_config_tiers() {
        let low = AdaptiveCompactConfig::from_pressure(0.3);
        assert_eq!(low.keep_recent, 6);
        assert_eq!(low.token_budget, 12_000);

        let med = AdaptiveCompactConfig::from_pressure(0.65);
        assert_eq!(med.keep_recent, 4);
        assert_eq!(med.token_budget, 8_000);

        let high = AdaptiveCompactConfig::from_pressure(0.80);
        assert_eq!(high.keep_recent, 2);
        assert_eq!(high.token_budget, 4_000);

        let extreme = AdaptiveCompactConfig::from_pressure(0.95);
        assert_eq!(extreme.keep_recent, 1);
        assert_eq!(extreme.token_budget, 2_000);
    }

    #[test]
    fn adaptive_compaction_more_aggressive_at_high_pressure() {
        let big = "x".repeat(6000); // ~1500 tokens each
        let mut msgs_low = vec![
            json!({"role": "user", "content": "task"}),
            assistant_with_tools(&[
                ("c1", "read_file"),
                ("c2", "read_file"),
                ("c3", "read_file"),
                ("c4", "read_file"),
                ("c5", "read_file"),
                ("c6", "read_file"),
                ("c7", "read_file"),
                ("c8", "read_file"),
            ]),
            tool_result("c1", &big),
            tool_result("c2", &big),
            tool_result("c3", &big),
            tool_result("c4", &big),
            tool_result("c5", &big),
            tool_result("c6", &big),
            tool_result("c7", &big),
            tool_result("c8", &big),
        ];
        let mut msgs_high = msgs_low.clone();

        let stats_low = compact_tool_results_adaptive(&mut msgs_low, 0.3, Default::default());
        let stats_high = compact_tool_results_adaptive(&mut msgs_high, 0.92, Default::default());

        assert!(
            stats_high.results_compacted > stats_low.results_compacted,
            "high pressure ({}) should compact more than low ({})",
            stats_high.results_compacted,
            stats_low.results_compacted
        );
    }

    // ── State-Aware Compaction Tests ─────────────────────────────────

    #[test]
    fn state_aware_pins_active_files() {
        use crate::cloud_session_facts::{FileEntry, SessionFacts};
        let mut facts = SessionFacts {
            turn: 10,
            ..Default::default()
        };
        facts.active_files.push(FileEntry {
            path: "src/important.rs".to_string(),
            last_action: "write".to_string(),
            turn: 9, // recent
        });
        facts.active_files.push(FileEntry {
            path: "src/old.rs".to_string(),
            last_action: "read".to_string(),
            turn: 1, // old
        });

        // Build messages: 8 read_file results, some for pinned files
        let mut msgs: Vec<Value> = Vec::new();
        let files = [
            "src/important.rs",
            "src/a.rs",
            "src/b.rs",
            "src/c.rs",
            "src/d.rs",
            "src/e.rs",
            "src/f.rs",
            "src/old.rs",
        ];
        for (i, file) in files.iter().enumerate() {
            let call_id = format!("call_{i}");
            msgs.push(json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": call_id,
                    "function": { "name": "read_file" }
                }]
            }));
            // Content starts with file path (common pattern)
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "name": "read_file",
                "content": format!("{}\n{}", file, "x".repeat(2000))
            }));
        }

        let mut msgs_normal = msgs.clone();
        let stats_normal =
            compact_tool_results_adaptive(&mut msgs_normal, 0.80, Default::default());

        let mut msgs_aware = msgs;
        let stats_aware =
            compact_tool_results_state_aware(&mut msgs_aware, 0.80, &facts, 5, Default::default());

        // State-aware should compact fewer (pinned file preserved)
        // The important.rs file (turn 9, within 5-turn window) should be pinned
        let important_idx = msgs_aware.iter().position(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|c| c.starts_with("src/important.rs"))
                .unwrap_or(false)
        });
        assert!(
            important_idx.is_some(),
            "pinned file should still have content"
        );
        let content = msgs_aware[important_idx.unwrap()]
            .get("content")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(
            !super::is_cleared_content(content),
            "pinned file should NOT be cleared"
        );

        // old.rs (turn 1, outside 5-turn window) should NOT be pinned
        // It may or may not be compacted depending on count, but it's eligible
        assert!(
            stats_aware.results_compacted > 0,
            "should compact some results"
        );
        // Normal compaction doesn't know about pins, so it may compact more
        assert!(
            stats_normal.results_compacted >= stats_aware.results_compacted,
            "state-aware ({}) should compact ≤ normal ({})",
            stats_aware.results_compacted,
            stats_normal.results_compacted
        );
    }

    #[test]
    fn state_aware_pins_pending_task_relevant_files() {
        use crate::cloud_session_facts::{PlanFact, SessionFacts};

        let facts = SessionFacts {
            turn: 10,
            plan_state: Some(PlanFact {
                goal: "finish compaction".to_string(),
                completed: 1,
                total: 2,
                current_subtask: Some("preserve src/pending.rs while editing".to_string()),
            }),
            ..Default::default()
        };
        let big = "x".repeat(2000);
        let mut messages = vec![
            assistant_with_tool_args(&[
                ("c1", "read_file", r#"{"path":"src/pending.rs"}"#),
                ("c2", "read_file", r#"{"path":"src/old.rs"}"#),
                ("c3", "read_file", r#"{"path":"src/other.rs"}"#),
            ]),
            json!({
                "role": "tool",
                "tool_call_id": "c1",
                "name": "read_file",
                "content": format!("src/pending.rs\n{big}")
            }),
            json!({
                "role": "tool",
                "tool_call_id": "c2",
                "name": "read_file",
                "content": format!("src/old.rs\n{big}")
            }),
            json!({
                "role": "tool",
                "tool_call_id": "c3",
                "name": "read_file",
                "content": format!("src/other.rs\n{big}")
            }),
        ];

        let stats =
            compact_tool_results_state_aware(&mut messages, 0.95, &facts, 5, Default::default());

        assert!(stats.results_compacted > 0);
        let pending = messages[1]["content"].as_str().unwrap();
        assert!(
            pending.starts_with("src/pending.rs"),
            "pending-task-relevant file result must remain intact, got: {pending}"
        );
        assert!(
            !super::is_cleared_content(pending),
            "pending-task-relevant file result must not be compacted"
        );
    }

    #[test]
    fn state_aware_with_empty_facts_behaves_like_normal() {
        use crate::cloud_session_facts::SessionFacts;
        let facts = SessionFacts::default(); // no active files

        let mut msgs: Vec<Value> = Vec::new();
        for i in 0..8 {
            let call_id = format!("call_{i}");
            msgs.push(json!({
                "role": "assistant",
                "tool_calls": [{"id": call_id, "function": {"name": "read_file"}}]
            }));
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "name": "read_file",
                "content": format!("file_{i}.rs\n{}", "x".repeat(2000))
            }));
        }

        let mut msgs_normal = msgs.clone();
        let stats_normal =
            compact_tool_results_adaptive(&mut msgs_normal, 0.80, Default::default());

        let mut msgs_aware = msgs;
        let stats_aware =
            compact_tool_results_state_aware(&mut msgs_aware, 0.80, &facts, 5, Default::default());

        // With no active files, nothing is pinned — should compact same amount
        assert_eq!(
            stats_normal.results_compacted,
            stats_aware.results_compacted
        );
    }

    #[test]
    fn compact_tool_results_omits_empty_assistant_tool_calls() {
        let mut messages = vec![
            json!({"role": "assistant", "content": "done", "tool_calls": []}),
            json!({"role": "tool", "content": "src/main.rs\n".to_string() + &"x".repeat(500), "tool_call_id": "c1", "name": "read_file"}),
        ];

        let _ = compact_tool_results(&mut messages, Some(0), Default::default());

        assert!(messages[0].get("tool_calls").is_none(), "{messages:?}");
    }

    // ── Enhanced cleared placeholder tests ────────────────────────────────

    /// Helper: assistant message with tool calls that include arguments.
    fn assistant_with_tool_args(calls: &[(&str, &str, &str)]) -> Value {
        let tool_calls: Vec<Value> = calls
            .iter()
            .map(
                |(id, name, args)| json!({"id": id, "function": {"name": name, "arguments": args}}),
            )
            .collect();
        json!({"role": "assistant", "content": "", "tool_calls": tool_calls})
    }

    #[test]
    fn cleared_placeholder_contains_tool_name() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            assistant_with_tools(&[("c1", "read_file"), ("c2", "grep")]),
            tool_result("c1", &big),
            tool_result("c2", &big),
        ];

        compact_tool_results(&mut messages, Some(0), Default::default());

        let c1 = messages[1]["content"].as_str().unwrap();
        let c2 = messages[2]["content"].as_str().unwrap();
        assert!(
            c1.contains("read_file"),
            "cleared placeholder should contain tool name, got: {c1}"
        );
        assert!(
            c2.contains("grep"),
            "cleared placeholder should contain tool name, got: {c2}"
        );
    }

    #[test]
    fn cleared_placeholder_contains_args_preview() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            assistant_with_tool_args(&[(
                "c1",
                "read_file",
                r#"{"path":"rust/crates/astra-tools/src/shell_ops.rs","start_line":189}"#,
            )]),
            tool_result("c1", &big),
        ];

        compact_tool_results(&mut messages, Some(0), Default::default());

        let c1 = messages[1]["content"].as_str().unwrap();
        assert!(
            c1.contains("shell_ops.rs"),
            "cleared placeholder should contain file path from args, got: {c1}"
        );
    }

    #[test]
    fn cleared_placeholder_contains_args_for_bash() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            assistant_with_tool_args(&[(
                "c1",
                "grep",
                r#"{"pattern":"is_rm_catastrophic","path":"crates/"}"#,
            )]),
            tool_result("c1", &big),
        ];

        compact_tool_results(&mut messages, Some(0), Default::default());

        let c1 = messages[1]["content"].as_str().unwrap();
        assert!(
            c1.contains("is_rm_catastrophic"),
            "cleared placeholder should contain grep pattern, got: {c1}"
        );
    }

    #[test]
    fn cleared_placeholder_still_detected_as_cleared() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file")]),
            tool_result("c1", &big),
            tool_result("c2", &big),
        ];

        compact_tool_results(&mut messages, Some(0), Default::default());

        // After first compaction, run again — should not re-compact already cleared
        let stats = compact_tool_results(&mut messages, Some(0), Default::default());
        assert_eq!(
            stats.results_compacted, 0,
            "already-cleared results should not be re-compacted"
        );
    }

    #[test]
    fn cleared_placeholder_adaptive_also_enhanced() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            assistant_with_tool_args(&[
                ("c1", "read_file", r#"{"path":"src/main.rs"}"#),
                ("c2", "read_file", r#"{"path":"src/lib.rs"}"#),
            ]),
            tool_result("c1", &big),
            tool_result("c2", &big),
        ];

        // pressure 0.95 → keep=1, so c1 gets compacted
        compact_tool_results_adaptive(&mut messages, 0.95, Default::default());

        let c1 = messages[1]["content"].as_str().unwrap();
        assert!(
            c1.contains("read_file"),
            "adaptive compact should also use enhanced placeholder, got: {c1}"
        );
        assert!(
            c1.contains("main.rs"),
            "adaptive compact should include args preview, got: {c1}"
        );
    }

    #[test]
    fn cleared_placeholder_state_aware_also_enhanced() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            assistant_with_tool_args(&[
                ("c1", "read_file", r#"{"path":"src/lib.rs"}"#),
                ("c2", "read_file", r#"{"path":"src/main.rs"}"#),
            ]),
            tool_result("c1", &big),
            tool_result("c2", &big),
        ];

        let facts = crate::cloud_session_facts::SessionFacts::default();
        // pressure 0.95 → keep=1, so c1 gets compacted
        compact_tool_results_state_aware(&mut messages, 0.95, &facts, 5, Default::default());

        let c1 = messages[1]["content"].as_str().unwrap();
        assert!(
            c1.contains("read_file"),
            "state-aware compact should also use enhanced placeholder, got: {c1}"
        );
    }

    #[test]
    fn cleared_placeholder_truncates_long_args() {
        let big = "x".repeat(1000);
        let long_args =
            r#"{"command":"cd /home/user/very/long/path/to/project && grep -rn 'some_very_long_pattern_that_goes_on_and_on' src/ tests/ docs/ --include='*.rs' --include='*.toml' 2>/dev/null | head -50"}"#.to_string();
        let mut messages = vec![
            assistant_with_tool_args(&[("c1", "grep", &long_args)]),
            tool_result("c1", &big),
        ];

        compact_tool_results(&mut messages, Some(0), Default::default());

        let c1 = messages[1]["content"].as_str().unwrap();
        assert!(
            c1.len() < 300,
            "cleared placeholder should be compact, got {} chars: {c1}",
            c1.len()
        );
    }

    // ── Provider-aware strategy tests ──

    #[test]
    fn strategy_from_provider_hint() {
        assert_eq!(
            CompactStrategy::from_provider_hint("claude-sonnet-4-20250514"),
            CompactStrategy::Minimal
        );
        assert_eq!(
            CompactStrategy::from_provider_hint("anthropic"),
            CompactStrategy::Minimal
        );
        assert_eq!(
            CompactStrategy::from_provider_hint("gpt-4o"),
            CompactStrategy::Normalized
        );
        assert_eq!(
            CompactStrategy::from_provider_hint("glm-4-plus"),
            CompactStrategy::Normalized
        );
        assert_eq!(
            CompactStrategy::from_provider_hint("deepseek-chat"),
            CompactStrategy::Normalized
        );
        assert_eq!(
            CompactStrategy::from_provider_hint(""),
            CompactStrategy::Normalized
        );
    }

    #[test]
    fn provider_cache_strategy_exposes_provider_capabilities() {
        let anthropic = ProviderCacheStrategy::from_provider_hint("anthropic/claude-sonnet-4");
        assert_eq!(
            anthropic.prompt_cache_protocol,
            PromptCacheProtocol::AnthropicCacheControl
        );
        assert_eq!(anthropic.compact_strategy, CompactStrategy::Minimal);
        assert!(anthropic.supports_cache_control);
        assert!(anthropic.supports_cache_reference);
        assert!(anthropic.supports_cache_edits);

        let openai = ProviderCacheStrategy::from_provider_hint("openai/gpt-4o");
        assert_eq!(openai.prompt_cache_protocol, PromptCacheProtocol::Prefix);
        assert_eq!(openai.compact_strategy, CompactStrategy::Normalized);
        assert!(!openai.supports_cache_control);
        assert!(!openai.supports_cache_reference);
        assert!(!openai.supports_cache_edits);
    }

    #[test]
    fn explicit_provider_takes_precedence_over_claude_named_model() {
        // Known non-Anthropic providers override model name
        assert_eq!(
            CompactStrategy::from_provider_and_model(Some("openai"), Some("claude-sonnet-4")),
            CompactStrategy::Normalized
        );
        assert_eq!(
            ProviderCacheStrategy::from_provider_and_model(Some("anthropic"), Some("gpt-4o"))
                .prompt_cache_protocol,
            PromptCacheProtocol::AnthropicCacheControl
        );
        // Unknown proxy providers (openrouter, litellm) fall through to model detection
        assert_eq!(
            ProviderCacheStrategy::from_provider_and_model(
                Some("openrouter"),
                Some("claude-sonnet-4-20250514")
            )
            .prompt_cache_protocol,
            PromptCacheProtocol::AnthropicCacheControl
        );
    }

    #[test]
    fn minimal_strategy_produces_short_placeholder() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            assistant_with_tool_args(&[
                ("c1", "read_file", r#"{"path":"src/main.rs"}"#),
                ("c2", "read_file", r#"{"path":"src/lib.rs"}"#),
            ]),
            tool_result("c1", &big),
            tool_result("c2", &big),
        ];

        compact_tool_results_adaptive(&mut messages, 0.95, CompactStrategy::Minimal);

        let c1 = messages[1]["content"].as_str().unwrap();
        assert_eq!(
            c1, "[Cleared]",
            "Minimal strategy should produce short placeholder"
        );
    }

    #[test]
    fn normalized_strategy_includes_key_value_args() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            assistant_with_tool_args(&[
                ("c1", "read_file", r#"{"path":"src/main.rs"}"#),
                ("c2", "read_file", r#"{"path":"src/lib.rs"}"#),
            ]),
            tool_result("c1", &big),
            tool_result("c2", &big),
        ];

        compact_tool_results_adaptive(&mut messages, 0.95, CompactStrategy::Normalized);

        let c1 = messages[1]["content"].as_str().unwrap();
        assert!(
            c1.contains("read_file"),
            "should contain tool name, got: {c1}"
        );
        assert!(
            c1.contains("path=src/main.rs"),
            "should contain normalized args, got: {c1}"
        );
        // Must NOT contain raw JSON
        assert!(
            !c1.contains('{'),
            "should not contain raw JSON braces, got: {c1}"
        );
    }

    #[test]
    fn normalized_strategy_omits_volatile_command_field() {
        let big = "x".repeat(1000);
        // web_fetch is compactable; url and query are stable keys
        let mut messages = vec![
            assistant_with_tool_args(&[
                (
                    "c1",
                    "web_fetch",
                    r#"{"url":"https://example.com","query":"test"}"#,
                ),
                ("c2", "web_fetch", r#"{"url":"https://other.com"}"#),
            ]),
            tool_result("c1", &big),
            tool_result("c2", &big),
        ];

        compact_tool_results_adaptive(&mut messages, 0.95, CompactStrategy::Normalized);

        let c1 = messages[1]["content"].as_str().unwrap();
        assert!(
            c1.contains("url=https://example.com"),
            "should contain url, got: {c1}"
        );
        assert!(c1.contains("query=test"), "should contain query, got: {c1}");
    }

    #[test]
    fn normalized_strategy_excludes_raw_json_and_volatile_fields() {
        let big = "x".repeat(1000);
        let mut messages = vec![
            assistant_with_tool_args(&[
                (
                    "transient-call-id-123",
                    "read_file",
                    r#"{"request_id":"req-123","call_id":"call-volatile","timestamp":"2026-04-27T00:51:35+08:00","path":"src/main.rs","command":"cat src/main.rs","old_str":"secret_old","new_str":"secret_new"}"#,
                ),
                ("c2", "read_file", r#"{"path":"src/lib.rs"}"#),
            ]),
            tool_result("transient-call-id-123", &big),
            tool_result("c2", &big),
        ];

        compact_tool_results_adaptive(&mut messages, 0.95, CompactStrategy::Normalized);

        let placeholder = messages[1]["content"].as_str().unwrap();
        assert!(placeholder.contains("path=src/main.rs"));
        for forbidden in [
            "{",
            "}",
            "request_id",
            "req-123",
            "call-volatile",
            "timestamp",
            "2026-04-27",
            "command",
            "cat src/main.rs",
            "old_str",
            "secret_old",
            "new_str",
            "secret_new",
            "transient-call-id-123",
        ] {
            assert!(
                !placeholder.contains(forbidden),
                "normalized placeholder must exclude volatile/raw field `{forbidden}`, got: {placeholder}"
            );
        }
    }

    #[test]
    fn normalized_strategy_is_deterministic_across_json_key_order() {
        let a =
            super::normalize_args(r#"{"timestamp":"t1","pattern":"TODO","path":"src/main.rs"}"#);
        let b =
            super::normalize_args(r#"{"path":"src/main.rs","pattern":"TODO","timestamp":"t2"}"#);

        assert_eq!(a, "path=src/main.rs pattern=TODO");
        assert_eq!(a, b);
    }

    #[test]
    fn normalize_args_extracts_stable_keys() {
        let result = super::normalize_args(r#"{"path":"src/main.rs","content":"some data"}"#);
        assert_eq!(result, "path=src/main.rs");

        let result = super::normalize_args(r#"{"pattern":"TODO","path":"src/"}"#);
        assert!(result.contains("path=src/"));
        assert!(result.contains("pattern=TODO"));

        // Invalid JSON returns empty
        let result = super::normalize_args("not json");
        assert!(result.is_empty());
    }

    // ─── Optimization: compaction persists cleared content to disk ──────────

    #[test]
    fn adaptive_compaction_persists_cleared_content_to_disk() {
        let dir = std::env::temp_dir().join("mc_persist_test");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let big = "x".repeat(2000);
        let mut messages = vec![
            assistant_with_tools(&[
                ("c1", "read_file"),
                ("c2", "read_file"),
                ("c3", "read_file"),
                ("c4", "read_file"),
                ("c5", "read_file"),
                ("c6", "read_file"),
                ("c7", "read_file"),
                ("c8", "read_file"),
            ]),
            tool_result("c1", &big),
            tool_result("c2", &big),
            tool_result("c3", &big),
            tool_result("c4", &big),
            tool_result("c5", &big),
            tool_result("c6", &big),
            tool_result("c7", &big),
            tool_result("c8", &big),
        ];

        let stats = compact_tool_results_adaptive_with_persistence(
            &mut messages,
            0.3,
            CompactStrategy::Normalized,
            Some(&dir),
        );

        assert!(
            stats.results_compacted > 0,
            "should compact at least some results"
        );

        // Every compacted result should have its full content persisted to disk
        for msg in &messages {
            if msg.get("role").and_then(Value::as_str) != Some("tool") {
                continue;
            }
            let content = msg["content"].as_str().unwrap_or("");
            if !is_cleared_content(content) {
                continue;
            }
            let call_id = msg["tool_call_id"].as_str().unwrap();
            let recovered = crate::tool_result_storage::read_persisted_result(&dir, call_id);
            assert!(
                recovered.is_some(),
                "cleared result for {call_id} must have been persisted to disk"
            );
            assert_eq!(
                recovered.unwrap(),
                big,
                "persisted content must match original"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adaptive_compaction_without_persistence_works_as_before() {
        let big = "x".repeat(2000);
        let mut messages = vec![
            assistant_with_tools(&[
                ("c1", "read_file"),
                ("c2", "read_file"),
                ("c3", "read_file"),
                ("c4", "read_file"),
                ("c5", "read_file"),
                ("c6", "read_file"),
                ("c7", "read_file"),
                ("c8", "read_file"),
            ]),
            tool_result("c1", &big),
            tool_result("c2", &big),
            tool_result("c3", &big),
            tool_result("c4", &big),
            tool_result("c5", &big),
            tool_result("c6", &big),
            tool_result("c7", &big),
            tool_result("c8", &big),
        ];

        // session_dir=None → no persistence, just clear as before
        let stats = compact_tool_results_adaptive_with_persistence(
            &mut messages,
            0.3,
            CompactStrategy::Normalized,
            None,
        );
        assert!(stats.results_compacted > 0);

        // Cleared results should NOT have disk files (no session dir)
        for msg in &messages {
            if msg.get("role").and_then(Value::as_str) != Some("tool") {
                continue;
            }
            let content = msg["content"].as_str().unwrap_or("");
            if is_cleared_content(content) {
                // Placeholder should be the regular cleared placeholder (no disk reference)
                assert!(
                    !content.contains("persisted-output"),
                    "without session_dir, no disk reference should appear"
                );
            }
        }
    }
}
