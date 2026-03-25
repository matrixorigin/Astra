//! Tool selection strategies.
//!
//! # Architecture
//!
//! Tool selection is a **separate concern** from tool execution and LLM chat.
//! This module defines the `ToolSelector` trait and three strategies:
//!
//! - [`TfIdfSelector`]: Fast heuristic fallback using TF-IDF scoring (no LLM call).
//!   `ConversationState` is its private implementation detail — not a public API.
//!   **Do NOT add fields to ConversationState for new edge cases.**
//!
//! - [`LlmToolSelector`]: Asks a model to select tools from a compact catalog summary.
//!   Handles semantic understanding natively — "matrixone呢？" after a PR query
//!   is trivial for an LLM but impossible for heuristics.
//!
//! - [`FallbackSelector`]: Tries LLM first (accurate), falls back to TF-IDF (fast)
//!   if the LLM call fails or times out.
//!
//! # Design rationale
//!
//! ConversationState was a **leaky abstraction**: every new edge case required
//! adding a field (`is_github`, `is_fetch`, `recent_tools`, etc.), effectively
//! simulating a mini language model with struct fields. The correct fix is to
//! let the actual LLM handle semantic understanding, and keep heuristics only
//! as a fallback for when the LLM is unavailable.
//!
//! The next edge case should be handled by **improving the LLM prompt**, not
//! by adding a field to ConversationState.

use crate::tool_registry::{self, TOOL_CATALOG, ToolRegistry};
use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

// ─── Public types ────────────────────────────────────────────────────────────

/// Context provided to tool selectors. Open for extension without
/// modifying selector implementations (they use what they need).
pub struct SelectionContext<'a> {
    pub query: &'a str,
    pub turn_count: u32,
    pub recent_tools: &'a [String],
    pub budget_tokens: u32,
}

/// Result of tool selection.
#[derive(Debug, Clone)]
pub struct SelectionResult {
    /// Tool names selected (pinned tools always included by ToolRegistry).
    pub tool_names: Vec<String>,
    /// Which strategy produced this result (used in tests and logging).
    #[allow(dead_code)]
    pub strategy: &'static str,
    /// Token budget consumed by dynamic (non-pinned) tools.
    #[allow(dead_code)]
    pub budget_used: u32,
    /// True if the selector failed (timeout, error, empty) — signals fallback.
    pub failed: bool,
}

/// Strategy for selecting tools from the catalog.
#[async_trait]
pub trait ToolSelector: Send + Sync {
    async fn select(&self, ctx: &SelectionContext<'_>) -> SelectionResult;
}

// ─── TF-IDF selector (heuristic fallback) ────────────────────────────────────

/// Fast heuristic selector using TF-IDF scoring. No LLM call.
/// Wraps [`ToolRegistry`] — ConversationState is an internal detail.
pub struct TfIdfSelector {
    registry: ToolRegistry,
}

impl TfIdfSelector {
    pub fn new(registry: ToolRegistry) -> Self {
        Self { registry }
    }

    #[allow(dead_code)]
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }
}

#[async_trait]
impl ToolSelector for TfIdfSelector {
    async fn select(&self, ctx: &SelectionContext<'_>) -> SelectionResult {
        let (_schemas, report) = self.registry.select_with_report_ctx(
            ctx.query,
            ctx.turn_count,
            ctx.budget_tokens,
            ctx.recent_tools,
        );
        SelectionResult {
            tool_names: report.tools_selected,
            strategy: "tfidf",
            budget_used: report.budget_used,
            failed: false,
        }
    }
}

// ─── Compact tool catalog for LLM prompt ─────────────────────────────────────

/// One-line-per-tool catalog summary for the LLM tool selector.
/// ~200 tokens — much cheaper than sending full schemas (~3800 tokens).
fn build_catalog_summary() -> String {
    TOOL_CATALOG
        .iter()
        .map(|t| {
            format!(
                "- {}: {}",
                t.name,
                t.description.split('.').next().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// System prompt for the tool selection LLM call.
const TOOL_SELECT_SYSTEM: &str = "\
You are a tool selector. Given the user's query and context, decide which tools are needed.
Return ONLY a JSON array of tool names. Select 1-5 tools. Do not explain.
Pinned tools (bash, read_file, write_file, str_replace, list_dir, grep, glob) are always available — do NOT include them.
Only select from the dynamic tools listed below.";

fn build_tool_select_prompt(query: &str, recent_tools: &[String], catalog: &str) -> Vec<Value> {
    let system = format!("{}\n\nDynamic tools:\n{}", TOOL_SELECT_SYSTEM, catalog);
    let mut user_msg = format!("Query: {}", query);
    if !recent_tools.is_empty() {
        user_msg.push_str(&format!("\nRecently used: {:?}", recent_tools));
    }
    vec![
        serde_json::json!({"role": "system", "content": system}),
        serde_json::json!({"role": "user", "content": user_msg}),
    ]
}

/// Parse tool names from LLM response text.
/// Handles: `["a", "b"]`, markdown code blocks, trailing text.
fn parse_tool_names_from_llm(text: &str) -> Vec<String> {
    // Find the first JSON array in the text
    let trimmed = text.trim();
    let json_str = if let Some(start) = trimmed.find('[') {
        if let Some(end) = trimmed[start..].find(']') {
            &trimmed[start..start + end + 1]
        } else {
            return vec![];
        }
    } else {
        return vec![];
    };

    serde_json::from_str::<Vec<String>>(json_str).unwrap_or_default()
}

// ─── LLM-based tool selector ────────────────────────────────────────────────

/// LLM-based tool selector. Makes a lightweight chat/turn call to select tools.
///
/// Uses the same `{base}/chat/turn` endpoint as the main chat loop, but with:
/// - A compact system prompt (~200 tokens)
/// - No tool schemas (the LLM just generates text)
/// - A 5-second timeout (tool selection should be fast)
///
/// If the call fails, returns an empty result so the fallback can take over.
pub struct LlmToolSelector {
    client: reqwest::Client,
    base_url: String,
    token: String,
    model: Option<String>,
    catalog_summary: String,
}

impl LlmToolSelector {
    pub fn new(client: reqwest::Client, base_url: String, token: String) -> Self {
        let catalog_summary = build_catalog_summary();
        Self {
            client,
            base_url,
            token,
            model: None,
            catalog_summary,
        }
    }

    #[allow(dead_code)]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Make a lightweight SSE call and collect the full response text.
    async fn call_llm(&self, messages: Vec<Value>) -> Result<String, String> {
        let mut payload = serde_json::json!({
            "messages": messages,
        });
        if let Some(ref model) = self.model {
            payload["model"] = Value::String(model.clone());
        }

        let resp = self
            .client
            .post(format!("{}/chat/turn", self.base_url))
            .header("authorization", format!("Bearer {}", self.token))
            .header("accept", "text/event-stream")
            .json(&payload)
            .timeout(Duration::from_secs(8))
            .send()
            .await
            .map_err(|e| format!("tool-select LLM call failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("tool-select LLM error {status}: {body}"));
        }

        // Collect SSE text events
        let full_body = resp.text().await.map_err(|e| e.to_string())?;
        let mut text = String::new();
        for line in full_body.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    break;
                }
                if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                    // Standard OpenAI streaming format
                    if let Some(delta) = chunk
                        .get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("delta"))
                        .and_then(|d| d.get("content"))
                        .and_then(Value::as_str)
                    {
                        text.push_str(delta);
                    }
                    // InProcess bridge format
                    if let Some(t) = chunk.get("type").and_then(Value::as_str) {
                        if t == "text_delta"
                            && let Some(d) = chunk.get("text").and_then(Value::as_str)
                        {
                            text.push_str(d);
                        }
                        if t == "_inprocess_summary"
                            && let Some(ft) = chunk.get("full_text").and_then(Value::as_str)
                        {
                            return Ok(ft.to_string());
                        }
                    }
                }
            }
        }
        Ok(text)
    }
}

#[async_trait]
impl ToolSelector for LlmToolSelector {
    async fn select(&self, ctx: &SelectionContext<'_>) -> SelectionResult {
        let messages = build_tool_select_prompt(ctx.query, ctx.recent_tools, &self.catalog_summary);

        match self.call_llm(messages).await {
            Ok(text) => {
                let mut names = parse_tool_names_from_llm(&text);
                // Validate: only keep names that exist in TOOL_CATALOG
                let valid: std::collections::HashSet<&str> =
                    TOOL_CATALOG.iter().map(|t| t.name).collect();
                names.retain(|n| valid.contains(n.as_str()));

                if names.is_empty() {
                    // LLM returned nothing useful — signal fallback
                    return SelectionResult {
                        tool_names: vec![],
                        strategy: "llm_empty",
                        budget_used: 0,
                        failed: true,
                    };
                }

                SelectionResult {
                    tool_names: names,
                    strategy: "llm",
                    budget_used: 0, // caller computes from actual schemas
                    failed: false,
                }
            }
            Err(_e) => {
                // LLM call failed — signal fallback
                SelectionResult {
                    tool_names: vec![],
                    strategy: "llm_error",
                    budget_used: 0,
                    failed: true,
                }
            }
        }
    }
}

// ─── Fallback selector ──────────────────────────────────────────────────────

/// Chained selector: tries `primary`, falls back to `fallback` if primary
/// returns empty or errors.
pub struct FallbackSelector {
    primary: Box<dyn ToolSelector>,
    fallback: Box<dyn ToolSelector>,
}

impl FallbackSelector {
    pub fn new(primary: Box<dyn ToolSelector>, fallback: Box<dyn ToolSelector>) -> Self {
        Self { primary, fallback }
    }
}

#[async_trait]
impl ToolSelector for FallbackSelector {
    async fn select(&self, ctx: &SelectionContext<'_>) -> SelectionResult {
        let result = self.primary.select(ctx).await;
        if !result.failed && !result.tool_names.is_empty() {
            result
        } else {
            self.fallback.select(ctx).await
        }
    }
}

// ─── Helpers for callers ────────────────────────────────────────────────────

/// Given selected tool names from a [`ToolSelector`], resolve them to full
/// JSON schemas from the registry. Pinned tools are always included.
pub fn resolve_schemas(
    registry: &ToolRegistry,
    selected_names: &[String],
) -> (Vec<Value>, tool_registry::SelectionReport) {
    let all_schemas = registry.all_tool_schemas();

    let mut schemas = Vec::new();
    let mut names = Vec::new();

    // Always include pinned tools first
    for tool in TOOL_CATALOG.iter() {
        if tool.pinned
            && let Some(schema) = all_schemas.iter().find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    == Some(tool.name)
            })
        {
            schemas.push(schema.clone());
            names.push(tool.name.to_string());
        }
    }

    // Add dynamic tools from selection
    for name in selected_names {
        if names.contains(name) {
            continue; // already included as pinned
        }
        if let Some(schema) = all_schemas.iter().find(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some(name.as_str())
        }) {
            schemas.push(schema.clone());
            names.push(name.clone());
        }
    }

    let budget_used: u32 = names
        .iter()
        .filter(|n| {
            !TOOL_CATALOG
                .iter()
                .any(|t| t.pinned && t.name == n.as_str())
        })
        .map(|n| registry.token_cost(n))
        .sum();

    let report = tool_registry::SelectionReport {
        tools_selected: names,
        selected_count: schemas.len() as u32,
        budget_used,
        budget_total: registry.default_budget(),
    };

    (schemas, report)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_registry::{ConversationState, IntentType, TOOL_CATALOG, pre_filter_dynamic};

    fn mock_registry() -> ToolRegistry {
        let schemas: Vec<Value> = TOOL_CATALOG
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": {"type": "object", "properties": {}}
                    }
                })
            })
            .collect();
        ToolRegistry::new(schemas)
    }

    // ── Parse LLM response ──

    #[test]
    fn parse_json_array() {
        let names = parse_tool_names_from_llm(r#"["github_list_prs", "memory_search"]"#);
        assert_eq!(names, vec!["github_list_prs", "memory_search"]);
    }

    #[test]
    fn parse_json_in_markdown_block() {
        let text = "```json\n[\"git_log\", \"git_diff\"]\n```";
        let names = parse_tool_names_from_llm(text);
        assert_eq!(names, vec!["git_log", "git_diff"]);
    }

    #[test]
    fn parse_json_with_trailing_text() {
        let text = "Based on the query, I'd select:\n[\"github_list_prs\"]\nThese tools...";
        let names = parse_tool_names_from_llm(text);
        assert_eq!(names, vec!["github_list_prs"]);
    }

    #[test]
    fn parse_no_json_returns_empty() {
        let names = parse_tool_names_from_llm("I don't know which tools to use");
        assert!(names.is_empty());
    }

    #[test]
    fn parse_malformed_json_returns_empty() {
        let names = parse_tool_names_from_llm("[github_list_prs]");
        assert!(names.is_empty());
    }

    // ── Catalog summary ──

    #[test]
    fn catalog_summary_covers_all_dynamic_tools() {
        let summary = build_catalog_summary();
        for tool in TOOL_CATALOG.iter().filter(|t| !t.pinned) {
            assert!(
                summary.contains(tool.name),
                "catalog summary missing tool: {}",
                tool.name
            );
        }
    }

    #[test]
    fn catalog_summary_is_compact() {
        let summary = build_catalog_summary();
        // Should be under 1000 chars (~250 tokens)
        assert!(
            summary.len() < 2000,
            "catalog summary too long: {} chars",
            summary.len()
        );
    }

    // ── Prompt construction ──

    #[test]
    fn prompt_includes_recent_tools() {
        let messages =
            build_tool_select_prompt("matrixone呢？", &["github_list_prs".to_string()], "catalog");
        let user_msg = messages[1]["content"].as_str().unwrap();
        assert!(user_msg.contains("github_list_prs"));
        assert!(user_msg.contains("matrixone"));
    }

    // ── TfIdfSelector ──

    #[tokio::test]
    async fn tfidf_pr_query_includes_github() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "matrixorigin memoria 最新的pr?",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
        };
        let result = selector.select(&ctx).await;
        assert!(
            result.tool_names.contains(&"github_list_prs".to_string()),
            "PR query must select github_list_prs, got: {:?}",
            result.tool_names
        );
        assert_eq!(result.strategy, "tfidf");
    }

    #[tokio::test]
    async fn tfidf_conversational_only_pinned() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "你好",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
        };
        let result = selector.select(&ctx).await;
        let dynamic_count = result
            .tool_names
            .iter()
            .filter(|n| {
                !TOOL_CATALOG
                    .iter()
                    .any(|t| t.pinned && t.name == n.as_str())
            })
            .count();
        assert_eq!(
            dynamic_count, 0,
            "conversational should have 0 dynamic tools"
        );
    }

    // ── Precision tests (the user's exact scenarios) ──

    #[test]
    fn prefilter_github_query_ranks_github_tools_first() {
        let state = ConversationState::from_message("milvus 的 PR", 1);
        let ranked = pre_filter_dynamic(&state, "milvus 的 PR");

        let top3_names: Vec<_> = ranked
            .iter()
            .take(3)
            .map(|&(idx, _)| TOOL_CATALOG[idx].name)
            .collect();
        assert!(
            top3_names.contains(&"github_list_prs"),
            "github_list_prs should be in top 3 for PR query, got: {:?}",
            top3_names
        );
    }

    #[test]
    fn prefilter_recency_boost_promotes_recent_tool() {
        let state = ConversationState::from_message_with_context(
            "matrixone 呢？",
            2,
            &["github_list_prs".to_string()],
        );
        let ranked = pre_filter_dynamic(&state, "matrixone 呢？");

        let prs_rank = ranked
            .iter()
            .position(|&(idx, _)| TOOL_CATALOG[idx].name == "github_list_prs");
        assert!(
            prs_rank.is_some_and(|r| r < 3),
            "recency boost should promote github_list_prs to top 3, got rank: {:?}",
            prs_rank
        );
    }

    #[test]
    fn prefilter_threshold_filters_irrelevant_tools() {
        let state = ConversationState::from_message("帮我写个排序算法", 1);
        let ranked = pre_filter_dynamic(&state, "帮我写个排序算法");

        let has_github = ranked
            .iter()
            .any(|&(idx, _)| TOOL_CATALOG[idx].intents.contains(&IntentType::GitHub));
        assert!(
            !has_github,
            "GitHub tools should be filtered for non-GitHub query"
        );
    }

    #[test]
    fn prefilter_memory_query_includes_memory_tools() {
        let state = ConversationState::from_message("我有哪些记忆？", 1);
        let ranked = pre_filter_dynamic(&state, "我有哪些记忆？");

        let has_memory = ranked
            .iter()
            .any(|&(idx, _)| TOOL_CATALOG[idx].intents.contains(&IntentType::Memory));
        assert!(has_memory, "Memory query should include memory tools");
    }

    #[test]
    fn prefilter_ci_query_includes_github_ci() {
        let state = ConversationState::from_message("memoria最新的ci?", 1);
        let ranked = pre_filter_dynamic(&state, "memoria最新的ci?");

        let top5_names: Vec<_> = ranked
            .iter()
            .take(5)
            .map(|&(idx, _)| TOOL_CATALOG[idx].name)
            .collect();
        assert!(
            top5_names.contains(&"github_ci_status"),
            "CI query should include github_ci_status in top 5, got: {:?}",
            top5_names
        );
    }

    // ── Resolve schemas ──

    #[test]
    fn resolve_schemas_always_includes_pinned() {
        let registry = mock_registry();
        let (schemas, report) = resolve_schemas(&registry, &["github_list_prs".into()]);

        let names = &report.tools_selected;
        assert!(names.contains(&"bash".to_string()), "must include bash");
        assert!(
            names.contains(&"read_file".to_string()),
            "must include read_file"
        );
        assert!(
            names.contains(&"github_list_prs".to_string()),
            "must include requested tool"
        );
        assert_eq!(schemas.len(), report.selected_count as usize);
    }

    #[test]
    fn resolve_schemas_deduplicates_pinned() {
        let registry = mock_registry();
        // Request bash (which is pinned) — should not appear twice
        let (_, report) = resolve_schemas(&registry, &["bash".into(), "github_list_prs".into()]);
        let bash_count = report
            .tools_selected
            .iter()
            .filter(|n| *n == "bash")
            .count();
        assert_eq!(bash_count, 1, "bash should appear exactly once");
    }

    // ── FallbackSelector ──

    #[tokio::test]
    async fn fallback_uses_primary_when_successful() {
        struct FixedSelector(Vec<String>);
        #[async_trait]
        impl ToolSelector for FixedSelector {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                SelectionResult {
                    tool_names: self.0.clone(),
                    strategy: "fixed",
                    budget_used: 0,
                    failed: false,
                }
            }
        }

        let primary = Box::new(FixedSelector(vec!["github_list_prs".into()]));
        let fallback = Box::new(FixedSelector(vec!["memory_search".into()]));
        let selector = FallbackSelector::new(primary, fallback);

        let ctx = SelectionContext {
            query: "test",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
        };
        let result = selector.select(&ctx).await;
        assert_eq!(result.strategy, "fixed");
        assert_eq!(result.tool_names, vec!["github_list_prs"]);
    }

    #[tokio::test]
    async fn fallback_uses_secondary_on_empty() {
        struct EmptySelector;
        #[async_trait]
        impl ToolSelector for EmptySelector {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                SelectionResult {
                    tool_names: vec![],
                    strategy: "llm_error",
                    budget_used: 0,
                    failed: true,
                }
            }
        }
        struct FixedSelector(Vec<String>);
        #[async_trait]
        impl ToolSelector for FixedSelector {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                SelectionResult {
                    tool_names: self.0.clone(),
                    strategy: "tfidf",
                    budget_used: 100,
                    failed: false,
                }
            }
        }

        let primary = Box::new(EmptySelector);
        let fallback = Box::new(FixedSelector(vec!["memory_search".into()]));
        let selector = FallbackSelector::new(primary, fallback);

        let ctx = SelectionContext {
            query: "test",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
        };
        let result = selector.select(&ctx).await;
        assert_eq!(result.strategy, "tfidf");
        assert_eq!(result.tool_names, vec!["memory_search"]);
    }
}
