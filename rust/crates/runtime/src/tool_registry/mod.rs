//! Intelligent tool selection: registry, pre-filter, semantic retrieval, budget gate.
//!
//! Replaces the binary `classify_tool_filter()` approach with a layered architecture:
//!
//! 1. **Pinned tools** — always included (bash, read_file, etc.), no selection budget
//! 2. **Pre-filter** — reorder dynamic tools by conversation state signals + tags (never remove)
//! 3. **Semantic retrieval** — embedding-based top-K selection from pre-filtered pool
//! 4. **Budget gate** — enforce token budget, greedily fill from ranked list
//!
//! Key invariant: pre-filter NEVER removes tools, only reorders. This structurally
//! guarantees no false-positive tool stripping.

mod meta;
mod registry;
mod report;
mod scoring;
mod state;

pub use meta::{IntentType, Scope, TOOL_CATALOG, ToolMeta};
pub use registry::ToolRegistry;
pub use report::{SelectionFeedback, SelectionReport};
pub use scoring::{DEFAULT_TOOL_BUDGET_TOKENS, pre_filter_dynamic};
pub use state::ConversationState;

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use scoring::{tfidf_score, tokenize};
    use serde_json::Value;
    use serde_json::json;
    use state::word_boundary_match;
    fn mock_schemas() -> Vec<Value> {
        // Build schemas matching TOOL_CATALOG names
        TOOL_CATALOG
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": {"type": "object", "properties": {}}
                    }
                })
            })
            .collect()
    }

    // ── Catalog invariants ──

    #[test]
    fn catalog_has_21_tools() {
        assert_eq!(TOOL_CATALOG.len(), 21);
    }

    #[test]
    fn catalog_has_9_pinned() {
        assert_eq!(ToolRegistry::pinned_count(), 9);
    }

    #[test]
    fn catalog_has_12_dynamic() {
        assert_eq!(ToolRegistry::dynamic_count(), 12);
    }

    #[test]
    fn pinned_tools_are_core_set() {
        let pinned: Vec<&str> = TOOL_CATALOG
            .iter()
            .filter(|t| t.pinned)
            .map(|t| t.name)
            .collect();
        assert!(pinned.contains(&"bash"));
        assert!(pinned.contains(&"read_file"));
        assert!(pinned.contains(&"write_file"));
        assert!(pinned.contains(&"str_replace"));
        assert!(pinned.contains(&"list_dir"));
        assert!(pinned.contains(&"grep"));
        assert!(pinned.contains(&"glob"));
        assert!(pinned.contains(&"memory_store"));
        assert!(pinned.contains(&"memory_search"));
    }

    #[test]
    fn all_tool_names_match_catalog() {
        let names = ToolRegistry::all_tool_names();
        assert_eq!(names.len(), TOOL_CATALOG.len());
        for tool in TOOL_CATALOG {
            assert!(names.contains(&tool.name), "missing: {}", tool.name);
        }
    }

    #[test]
    fn every_tool_has_triggers() {
        for tool in TOOL_CATALOG {
            assert!(!tool.triggers.is_empty(), "{} has no triggers", tool.name);
        }
    }

    #[test]
    fn every_tool_has_intents() {
        for tool in TOOL_CATALOG {
            assert!(!tool.intents.is_empty(), "{} has no intents", tool.name);
        }
    }

    // ── ConversationState extraction ──

    #[test]
    fn state_detects_fetch() {
        let state = ConversationState::from_message("matrixorigin memoria 最新的pr?", 1);
        assert!(state.is_fetch, "should detect fetch from '最新'");
    }

    #[test]
    fn state_detects_mutate() {
        let state = ConversationState::from_message("create a new issue for the bug", 1);
        assert!(state.is_mutate, "should detect mutation");
    }

    #[test]
    fn state_detects_history_ref() {
        let state = ConversationState::from_message("分析一下之前的决策", 1);
        assert!(state.references_history, "should detect history reference");
    }

    #[test]
    fn state_detects_analytical() {
        let state = ConversationState::from_message("为什么选错了工具", 1);
        assert!(state.is_analytical, "should detect analytical intent");
    }

    #[test]
    fn state_detects_conversational() {
        let state = ConversationState::from_message("谢谢", 1);
        assert!(state.is_conversational, "should detect conversational");
    }

    #[test]
    fn state_long_message_not_conversational() {
        let state = ConversationState::from_message(
            "thank you for that, now please fix the test in main.rs",
            1,
        );
        assert!(
            !state.is_conversational,
            "long message should not be conversational"
        );
    }

    // ── Pre-filter ordering ──

    #[test]
    fn prefilter_ranks_github_tools_for_pr_query() {
        let state = ConversationState::from_message("matrixorigin memoria 最新的pr?", 1);
        let ranked = pre_filter_dynamic(&state, "matrixorigin memoria 最新的pr?");

        // github_list_prs should be ranked first among dynamic tools
        let first_name = TOOL_CATALOG[ranked[0].0].name;
        assert_eq!(
            first_name, "github_list_prs",
            "github_list_prs should be top-ranked for PR query, got: {}",
            first_name
        );
    }

    #[test]
    fn prefilter_ranks_memory_purge_for_recall() {
        // memory_store and memory_search are pinned (always available).
        // memory_purge is dynamic and should appear for memory-related queries.
        let state = ConversationState::from_message("之前我记住了什么偏好?", 1);
        let ranked = pre_filter_dynamic(&state, "之前我记住了什么偏好?");

        let top_names: Vec<&str> = ranked
            .iter()
            .take(5)
            .map(|&(idx, _)| TOOL_CATALOG[idx].name)
            .collect();
        assert!(
            top_names.contains(&"memory_purge"),
            "memory_purge should appear for recall query, got: {:?}",
            top_names
        );
    }

    /// Regression: "我有哪些记忆？" — memory_store and memory_search are now pinned,
    /// so they're always available. Verify via registry.select().
    #[test]
    fn select_memory_query_includes_pinned_memory_tools() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.select("我有哪些记忆？", 1);
        let names = ToolRegistry::selected_names(&selected);
        assert!(
            names.contains(&"memory_store".to_string()),
            "memory_store (pinned) must be in selection for '我有哪些记忆？', got: {:?}",
            names
        );
        assert!(
            names.contains(&"memory_search".to_string()),
            "memory_search (pinned) must be in selection for '我有哪些记忆？', got: {:?}",
            names
        );
    }

    /// Regression: "苹果比较好吃" is a preference statement with zero keyword overlap
    /// with memory_store triggers. Before pinning, memory_store was filtered out and
    /// the LLM couldn't store the preference despite system prompt instructions.
    #[test]
    fn select_preference_statement_has_memory_store() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.select("苹果比较好吃", 1);
        let names = ToolRegistry::selected_names(&selected);
        assert!(
            names.contains(&"memory_store".to_string()),
            "memory_store (pinned) must be available for preference statements, got: {:?}",
            names
        );
    }

    /// Conversational greetings should produce no dynamic tools (save tokens).
    #[test]
    fn prefilter_greeting_returns_empty() {
        let state = ConversationState::from_message("hi", 1);
        let ranked = pre_filter_dynamic(&state, "hi");
        assert!(
            ranked.is_empty(),
            "pure greeting should skip dynamic tools, got {} tools",
            ranked.len()
        );
    }

    /// CJK bigram tokenizer produces "记忆" bigram from "记忆"
    #[test]
    fn tokenize_cjk_bigrams() {
        let terms = tokenize("我有哪些记忆");
        assert!(
            terms.contains(&"记忆".to_string()),
            "tokenizer should emit CJK bigram '记忆', got: {:?}",
            terms
        );
        // Also check unigrams present
        assert!(terms.contains(&"记".to_string()));
        assert!(terms.contains(&"忆".to_string()));
    }

    #[test]
    fn prefilter_ranks_git_tools_for_diff() {
        let state = ConversationState::from_message("show me the git diff", 1);
        let ranked = pre_filter_dynamic(&state, "show me the git diff");

        let top_names: Vec<&str> = ranked
            .iter()
            .take(3)
            .map(|&(idx, _)| TOOL_CATALOG[idx].name)
            .collect();
        assert!(
            top_names.contains(&"git_diff"),
            "git_diff should be top-3, got: {:?}",
            top_names
        );
    }

    #[test]
    fn prefilter_never_returns_empty() {
        // Even for completely unknown queries, the fallback ensures at least 3 tools.
        let state = ConversationState::from_message("something completely random xyz", 1);
        let ranked = pre_filter_dynamic(&state, "something completely random xyz");
        assert!(
            !ranked.is_empty(),
            "pre-filter must never return empty (fallback to top-3 applies)"
        );
        assert!(
            ranked.len() <= ToolRegistry::dynamic_count(),
            "pre-filter must not exceed total dynamic count"
        );
    }

    #[test]
    fn prefilter_filters_by_score_threshold() {
        // A query with no matching intent should produce a filtered subset (not all tools).
        let state = ConversationState::from_message("test", 1);
        let ranked = pre_filter_dynamic(&state, "test");
        // Should be fewer than all dynamic tools due to score threshold.
        // The fallback ensures at least 3 results.
        assert!(
            !ranked.is_empty(),
            "pre-filter must return at least 3 tools via fallback"
        );
    }

    // ── Budget gate ──

    #[test]
    fn budget_always_includes_pinned() {
        let registry = ToolRegistry::new(mock_schemas());
        let result = registry.select_with_budget("你好", 1, 0);
        let names = ToolRegistry::selected_names(&result);
        assert!(
            names.contains(&"bash".to_string()),
            "bash must always be included"
        );
        assert!(
            names.contains(&"read_file".to_string()),
            "read_file must always be included"
        );
        assert_eq!(names.len(), ToolRegistry::pinned_count());
    }

    #[test]
    fn budget_respects_limit() {
        let registry = ToolRegistry::new(mock_schemas());
        // Very small budget — should include fewer dynamic tools
        let result = registry.select_with_budget("最新的pr?", 1, 50);
        let total_dynamic = result.len() - ToolRegistry::pinned_count();
        assert!(
            total_dynamic <= 2,
            "50 token budget should fit ≤2 dynamic tools, got {}",
            total_dynamic
        );
    }

    #[test]
    fn budget_large_includes_relevant_tools() {
        let registry = ToolRegistry::new(mock_schemas());
        // A GitHub-specific query with huge budget: should include GitHub tools.
        // No longer guarantees ALL tools since score threshold filters irrelevant ones.
        let result = registry.select_with_budget("最新的pr?", 1, 10000);
        let names = ToolRegistry::selected_names(&result);
        assert!(
            names.contains(&"github_list_prs".to_string()),
            "github_list_prs should be included for PR query"
        );
        // At minimum pinned + at least 1 dynamic tool
        assert!(
            result.len() > ToolRegistry::pinned_count(),
            "large budget should include at least some dynamic tools"
        );
    }

    // ── ToolRegistry integration ──

    #[test]
    fn registry_select_pr_query_includes_github() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.select("matrixorigin memoria 最新的pr?", 1);
        let names = ToolRegistry::selected_names(&selected);
        assert!(
            names.contains(&"github_list_prs".to_string()),
            "PR query must include github_list_prs, got: {:?}",
            names
        );
        // Pinned always present
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"read_file".to_string()));
    }

    #[test]
    fn registry_select_conversational_only_pinned() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.select("你好", 1);
        let names = ToolRegistry::selected_names(&selected);
        assert_eq!(
            names.len(),
            ToolRegistry::pinned_count(),
            "conversational query should only have pinned tools, got: {:?}",
            names
        );
    }

    #[test]
    fn registry_select_complex_query() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.select("analyze why the CI failed on the latest PR", 1);
        let names = ToolRegistry::selected_names(&selected);
        // Should include both GitHub tools and git tools
        assert!(
            names.contains(&"github_ci_status".to_string())
                || names.contains(&"github_list_prs".to_string()),
            "CI/PR query should include GitHub tools, got: {:?}",
            names
        );
    }

    #[test]
    fn registry_select_memory_query() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.select("我之前记住的偏好是什么?", 1);
        let names = ToolRegistry::selected_names(&selected);
        assert!(
            names.contains(&"memory_search".to_string()),
            "memory query should include memory_search, got: {:?}",
            names
        );
    }

    #[test]
    fn registry_select_create_issue() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.select("create a new issue for this bug", 1);
        let names = ToolRegistry::selected_names(&selected);
        assert!(
            names.contains(&"github_create_issue".to_string()),
            "create issue query should include github_create_issue, got: {:?}",
            names
        );
    }

    #[test]
    fn registry_select_git_status() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.select("git status 看看改了什么", 1);
        let names = ToolRegistry::selected_names(&selected);
        assert!(
            names.contains(&"git_status".to_string()),
            "git status query should include git_status, got: {:?}",
            names
        );
    }

    #[test]
    fn registry_select_reflect_query() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.select("为什么上次选错了工具?", 1);
        let names = ToolRegistry::selected_names(&selected);
        assert!(
            names.contains(&"reflect".to_string()),
            "reflect query should include reflect, got: {:?}",
            names
        );
    }

    // ── Word boundary matching ──

    #[test]
    fn word_boundary_match_basic() {
        assert!(word_boundary_match("show me the pr", &[], "pr"));
        assert!(word_boundary_match("pr list", &[], "pr"));
        assert!(!word_boundary_match("spray", &[], "pr"));
        assert!(!word_boundary_match("express", &[], "pr"));
    }

    #[test]
    fn word_boundary_match_cjk_adjacent() {
        // CJK chars are not ASCII word chars, so they act as boundaries
        assert!(word_boundary_match("最新的pr", &[], "pr"));
        assert!(word_boundary_match("看pr详情", &[], "pr"));
    }

    // ── select_with_budget ──

    #[test]
    fn select_with_budget_larger_returns_more_tools() {
        let schemas = mock_schemas();
        let registry = ToolRegistry::new(schemas);
        let small = registry.select_with_budget("matrixorigin memoria 最新的pr?", 1, 500);
        let large = registry.select_with_budget("matrixorigin memoria 最新的pr?", 1, 6000);
        assert!(
            large.len() >= small.len(),
            "larger budget should include at least as many tools"
        );
    }

    #[test]
    fn select_with_budget_zero_still_returns_pinned() {
        let schemas = mock_schemas();
        let registry = ToolRegistry::new(schemas);
        let selected = registry.select_with_budget("matrixorigin memoria 最新的pr?", 1, 0);
        // Pinned tools are budget-exempt, always included
        assert_eq!(selected.len(), ToolRegistry::pinned_count());
    }

    // ── TF-IDF scoring ──

    #[test]
    fn tokenize_mixed_cjk_ascii() {
        let terms = tokenize("matrixorigin memoria 最新的pr?");
        assert!(terms.contains(&"matrixorigin".to_string()));
        assert!(terms.contains(&"memoria".to_string()));
        assert!(terms.contains(&"最".to_string()));
        assert!(terms.contains(&"新".to_string()));
        assert!(terms.contains(&"的".to_string()));
        assert!(terms.contains(&"pr".to_string()));
    }

    #[test]
    fn tfidf_github_tools_rank_high_for_pr_query() {
        let terms = tokenize("list pull requests");
        let prs_idx = TOOL_CATALOG
            .iter()
            .position(|t| t.name == "github_list_prs")
            .unwrap();
        let git_idx = TOOL_CATALOG
            .iter()
            .position(|t| t.name == "git_status")
            .unwrap();
        assert!(
            tfidf_score(&terms, prs_idx) > tfidf_score(&terms, git_idx),
            "github_list_prs should score higher than git_status for 'list pull requests'"
        );
    }

    #[test]
    fn tfidf_memory_tools_rank_high_for_recall() {
        let terms = tokenize("search memory recall preferences");
        let mem_idx = TOOL_CATALOG
            .iter()
            .position(|t| t.name == "memory_search")
            .unwrap();
        let git_idx = TOOL_CATALOG
            .iter()
            .position(|t| t.name == "git_diff")
            .unwrap();
        assert!(
            tfidf_score(&terms, mem_idx) > tfidf_score(&terms, git_idx),
            "memory_search should score higher than git_diff for memory query"
        );
    }

    #[test]
    fn tfidf_git_tools_rank_high_for_diff() {
        let terms = tokenize("git diff");
        let diff_idx = TOOL_CATALOG
            .iter()
            .position(|t| t.name == "git_diff")
            .unwrap();
        let issue_idx = TOOL_CATALOG
            .iter()
            .position(|t| t.name == "github_list_issues")
            .unwrap();
        assert!(
            tfidf_score(&terms, diff_idx) > tfidf_score(&terms, issue_idx),
            "git_diff should score higher than github_list_issues for 'git diff'"
        );
    }

    #[test]
    fn state_detects_git_signal() {
        let state = ConversationState::from_message("show me the git diff", 1);
        assert!(state.is_git, "should detect git signal");
        assert!(state.is_fetch, "should detect fetch signal");
    }

    #[test]
    fn state_detects_github_signal() {
        let state = ConversationState::from_message("matrixorigin 最新的pr", 1);
        assert!(state.is_github, "should detect github signal from 'pr'");
    }

    // ── Real token measurement ──

    #[test]
    fn measured_costs_populated_for_all_tools() {
        let registry = ToolRegistry::new(mock_schemas());
        for tool in TOOL_CATALOG {
            let cost = registry.token_cost(tool.name);
            assert!(
                cost > 0,
                "tool {} should have positive token cost, got {}",
                tool.name,
                cost
            );
        }
    }

    #[test]
    fn measured_cost_uses_real_schema_size() {
        let schemas = mock_schemas();
        let registry = ToolRegistry::new(schemas.clone());
        // The real cost should be based on JSON bytes/4, not the static catalog estimate
        let bash_cost = registry.token_cost("bash");
        let bash_json = serde_json::to_string(
            schemas
                .iter()
                .find(|s| s["function"]["name"] == "bash")
                .unwrap(),
        )
        .unwrap();
        let expected = (bash_json.len() / 4) as u32;
        assert_eq!(
            bash_cost, expected,
            "measured cost should equal JSON bytes / 4"
        );
    }

    #[test]
    fn token_cost_falls_back_to_catalog_for_unknown() {
        let registry = ToolRegistry::new(vec![]); // No schemas
        let cost = registry.token_cost("nonexistent_tool");
        assert_eq!(cost, 40, "unknown tool should fall back to default 40");
    }

    // ── Selection report ──

    #[test]
    fn select_with_report_returns_consistent_data() {
        let registry = ToolRegistry::new(mock_schemas());
        let (schemas, report) = registry.select_with_report("matrixorigin 最新的pr?", 1, 3000);
        assert_eq!(schemas.len(), report.selected_count as usize);
        assert_eq!(
            ToolRegistry::selected_names(&schemas),
            report.tools_selected
        );
        assert_eq!(report.budget_total, 3000);
    }

    #[test]
    fn select_with_report_conversational_zero_budget() {
        let registry = ToolRegistry::new(mock_schemas());
        let (_schemas, report) = registry.select_with_report("你好", 1, 3000);
        assert_eq!(
            report.budget_used, 0,
            "conversational query should use 0 budget"
        );
        assert_eq!(report.selected_count as usize, ToolRegistry::pinned_count());
    }

    // ── Selection feedback ──

    #[test]
    fn feedback_perfect_precision() {
        let report = SelectionReport {
            tools_selected: vec!["bash".into(), "github_list_prs".into()],
            selected_count: 2,
            budget_used: 50,
            budget_total: 3000,
        };
        let fb = report.feedback(&["github_list_prs".into()]);
        assert_eq!(fb.precision, 1.0, "all used tools were selected");
        assert_eq!(fb.unused_count, 1, "bash was selected but not used");
    }

    #[test]
    fn feedback_no_tools_used() {
        let report = SelectionReport {
            tools_selected: vec!["bash".into(), "github_list_prs".into()],
            selected_count: 2,
            budget_used: 50,
            budget_total: 3000,
        };
        let fb = report.feedback(&[]);
        assert_eq!(
            fb.precision, 1.0,
            "empty usage = vacuously perfect precision"
        );
        assert_eq!(fb.unused_count, 2);
    }

    #[test]
    fn feedback_tool_not_in_selection() {
        let report = SelectionReport {
            tools_selected: vec!["bash".into()],
            selected_count: 1,
            budget_used: 30,
            budget_total: 3000,
        };
        let fb = report.feedback(&["github_list_prs".into()]);
        assert_eq!(fb.precision, 0.0, "used tool wasn't selected → precision 0");
        assert_eq!(fb.unused_count, 1, "bash selected but not used");
    }
}
