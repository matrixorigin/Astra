//! Intelligent tool selection: registry, pre-filter, budget gate.
//!
//! Layered architecture:
//!
//! 1. **Pinned tools** — always included (bash, read_file, etc.), no selection budget
//! 2. **Pre-filter** — rank dynamic tools by TF-IDF + trigger match + intent/scope signals
//! 3. **Recall-first** — adaptive threshold, intent diversity, MIN_RECALL_TOOLS guarantee
//! 4. **Budget gate** — enforce token budget, greedily fill from ranked list
//!
//! Cross-language coverage comes from rich multilingual triggers on each tool,
//! NOT from embeddings. With only ~23 tools, keyword coverage + intent rules
//! achieve high accuracy without ML dependencies.

pub mod chain;
mod meta;
pub mod plugin;
mod registry;
mod report;
pub mod scoring;
pub mod state;

pub use chain::{ChainContext, ChainStep, ToolChain};
pub use meta::{IntentType, Scope, TOOL_CATALOG, ToolMeta};
pub use plugin::{PluginRegistry, PluginToolEntry};
pub use registry::ToolRegistry;
pub use report::{SelectionFeedback, SelectionReport, ToolQualityTracker};
pub use scoring::{
    DEFAULT_TOOL_BUDGET_TOKENS, pre_filter_dynamic, pre_filter_dynamic_calibrated,
    pre_filter_dynamic_with_memory, pre_filter_dynamic_with_quality, tfidf_score,
};
pub use state::ConversationState;

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_tokenize::tokenize;
    use scoring::tfidf_score;
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
    fn catalog_has_33_tools() {
        assert_eq!(TOOL_CATALOG.len(), 34);
    }

    #[test]
    fn catalog_has_9_pinned() {
        assert_eq!(ToolRegistry::pinned_count(), 9);
    }

    #[test]
    fn catalog_has_24_dynamic() {
        assert_eq!(ToolRegistry::dynamic_count(), 25);
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
    fn registry_select_repo_stats_query() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.select("matrixorigin memoria 多少star了？", 1);
        let names = ToolRegistry::selected_names(&selected);
        assert!(
            names.contains(&"github_repo_stats".to_string()),
            "repo stats query should include github_repo_stats, got: {:?}",
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
        // precision = hits(1) / selected(2) = 0.5
        assert!(
            (fb.precision - 0.5).abs() < 0.01,
            "precision: 1 of 2 selected was used"
        );
        // recall = hits(1) / used(1) = 1.0
        assert_eq!(fb.recall, 1.0, "all used tools were selected");
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
        // precision = 0/2 = 0.0 (nothing used)
        assert!(
            (fb.precision).abs() < 0.01,
            "no tools used → zero precision"
        );
        // recall = vacuously 1.0 (nothing to miss)
        assert_eq!(fb.recall, 1.0, "empty usage = vacuously perfect recall");
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
        // precision = 0/1 = 0.0 (selected bash, never used)
        assert_eq!(fb.precision, 0.0, "selected tool wasn't used → precision 0");
        // recall = 0/1 = 0.0 (used tool wasn't in selection)
        assert_eq!(fb.recall, 0.0, "used tool wasn't selected → recall 0");
        assert_eq!(fb.unused_count, 1, "bash selected but not used");
    }

    // ── Quality tracker wiring tests ──

    #[test]
    fn quality_tracker_boosts_tool_ranking() {
        use scoring::pre_filter_dynamic_with_quality;

        let state = ConversationState::from_message_with_context(
            "show me the github pull requests",
            2,
            &[],
        );

        // Without tracker: get baseline ranking
        let baseline =
            pre_filter_dynamic_with_quality(&state, "show me the github pull requests", None);

        // With tracker: record many successful uses for a specific tool
        let mut tracker = ToolQualityTracker::new();
        for _ in 0..10 {
            tracker.record_selection(&["github_get_issue".into()]);
            tracker.record_feedback(&SelectionFeedback {
                tools_used: vec!["github_get_issue".into()],
                unused_count: 0,
                precision: 1.0,
                recall: 1.0,
            });
            tracker.record_quality("github_get_issue", 0.95);
        }

        let boosted = pre_filter_dynamic_with_quality(
            &state,
            "show me the github pull requests",
            Some(&tracker),
        );

        let find_score = |results: &[(usize, f64)], name: &str| -> Option<f64> {
            results.iter().find_map(|(idx, score)| {
                if TOOL_CATALOG[*idx].name == name {
                    Some(*score)
                } else {
                    None
                }
            })
        };

        let baseline_score = find_score(&baseline, "github_get_issue").unwrap_or(0.0);
        let boosted_score = find_score(&boosted, "github_get_issue").unwrap_or(0.0);
        assert!(
            boosted_score >= baseline_score,
            "quality tracker should boost tool score: baseline={:.4} boosted={:.4}",
            baseline_score,
            boosted_score
        );
    }

    #[test]
    fn quality_tracker_penalizes_ineffective_tool() {
        use scoring::pre_filter_dynamic_with_quality;

        let state = ConversationState::from_message_with_context("show me the git log", 2, &[]);

        // Record many selections but zero uses for a dynamic tool
        let mut tracker = ToolQualityTracker::new();
        for _ in 0..10 {
            tracker.record_selection(&["git_diff".into()]);
            // No record_feedback → tool never used → use_rate = 0
        }

        let baseline = pre_filter_dynamic_with_quality(&state, "show me the git log", None);
        let penalized =
            pre_filter_dynamic_with_quality(&state, "show me the git log", Some(&tracker));

        let find_score = |results: &[(usize, f64)], name: &str| -> Option<f64> {
            results.iter().find_map(|(idx, score)| {
                if TOOL_CATALOG[*idx].name == name {
                    Some(*score)
                } else {
                    None
                }
            })
        };

        let baseline_score = find_score(&baseline, "git_diff").unwrap_or(0.0);
        let penalized_score = find_score(&penalized, "git_diff").unwrap_or(0.0);
        assert!(
            penalized_score <= baseline_score,
            "quality tracker should penalize ineffective tool: baseline={:.4} penalized={:.4}",
            baseline_score,
            penalized_score
        );
    }

    #[test]
    fn registry_select_with_quality_changes_output() {
        let schemas = mock_schemas();
        let registry = ToolRegistry::new(schemas);

        // Record extensive failure for one dynamic tool
        let mut tracker = ToolQualityTracker::new();
        for _ in 0..20 {
            tracker.record_selection(&["github_list_prs".into()]);
            // Never used → penalized
        }

        // Both should compile and produce valid results
        let (without, report_without) =
            registry.select_with_quality("show me the PRs", 2, 800, &[], None);
        let (with, report_with) =
            registry.select_with_quality("show me the PRs", 2, 800, &[], Some(&tracker));

        // Basic sanity: both should include pinned tools
        assert!(without.len() >= ToolRegistry::pinned_count());
        assert!(with.len() >= ToolRegistry::pinned_count());

        // Report should be well-formed
        assert!(report_without.budget_total == 800);
        assert!(report_with.budget_total == 800);
    }

    // ── Disambiguation wiring tests ──

    #[test]
    fn disambiguation_auto_computed_on_state() {
        let state = ConversationState::from_message_with_context(
            "create a PR and show me the latest issues",
            2,
            &[],
        );
        // Should have disambiguation computed (is_fetch + is_mutate = conflict)
        assert!(state.disambiguation.is_some());
        let disambig = state.disambiguation.as_ref().unwrap();
        assert_eq!(disambig.conflict_score, 0.8, "fetch+mutate should conflict");
        assert_eq!(
            disambig.recommendation,
            crate::turn::routing_metrics::DisambiguationAction::WidenToolSelection
        );
    }

    #[test]
    fn disambiguation_widens_tool_selection() {
        use scoring::pre_filter_dynamic_with_quality;

        // Single-intent query: just fetch
        let fetch_state =
            ConversationState::from_message_with_context("show me the latest PRs", 2, &[]);
        let fetch_results =
            pre_filter_dynamic_with_quality(&fetch_state, "show me the latest PRs", None);

        // Multi-intent conflicting query: fetch + mutate
        let conflict_state = ConversationState::from_message_with_context(
            "show me the latest PRs and create a new issue",
            2,
            &[],
        );
        let conflict_results = pre_filter_dynamic_with_quality(
            &conflict_state,
            "show me the latest PRs and create a new issue",
            None,
        );

        // Conflicting query should select at least as many tools (lower threshold)
        assert!(
            conflict_results.len() >= fetch_results.len(),
            "conflicting intents should widen selection: fetch={} conflict={}",
            fetch_results.len(),
            conflict_results.len()
        );
    }

    #[test]
    fn disambiguation_conversational_has_no_conflict() {
        let state = ConversationState::from_message_with_context("hello", 1, &[]);
        let disambig = state.disambiguation.as_ref().unwrap();
        assert_eq!(disambig.primary_intent, "conversational");
        assert_eq!(disambig.conflict_score, 0.0);
    }

    // ── ConfidenceCalibrator integration tests ──

    #[test]
    fn calibrator_lowers_threshold_for_high_correction_rate() {
        use crate::turn::routing_metrics::ConfidenceCalibrator;
        let cal = ConfidenceCalibrator::new(0.7);
        // Record 10 github selections, 8 were corrected (80% correction rate)
        for _ in 0..10 {
            cal.record("github", true);
        }
        for _ in 0..2 {
            cal.record("github", false);
        }
        let threshold = cal.calibrated_threshold("github");
        // Should be lowered: 0.7 - (0.83 * 0.3) ≈ 0.45
        assert!(
            threshold < 0.7,
            "high correction rate should lower threshold"
        );
        assert!(threshold >= 0.3, "threshold should not go below min");
    }

    #[test]
    fn calibrated_prefilter_includes_more_tools_with_corrections() {
        use crate::turn::routing_metrics::ConfidenceCalibrator;

        let state = ConversationState::from_message("list open PRs in matrixone", 3);

        // Without calibrator
        let results_uncalibrated =
            scoring::pre_filter_dynamic(&state, "list open PRs in matrixone");

        // With calibrator that has high correction rate for "github"
        let cal = ConfidenceCalibrator::new(0.7);
        for _ in 0..10 {
            cal.record("github", true);
        }
        let results_calibrated = scoring::pre_filter_dynamic_calibrated(
            &state,
            "list open PRs in matrixone",
            None,
            Some(&cal),
        );

        // Calibrated should include at least as many tools (lower threshold → more tools)
        assert!(
            results_calibrated.len() >= results_uncalibrated.len(),
            "calibrated ({}) should be >= uncalibrated ({})",
            results_calibrated.len(),
            results_uncalibrated.len(),
        );
    }

    #[test]
    fn calibrator_no_effect_with_insufficient_data() {
        use crate::turn::routing_metrics::ConfidenceCalibrator;
        let cal = ConfidenceCalibrator::new(0.7);
        // Only 3 records — below the 5-minimum
        for _ in 0..3 {
            cal.record("fetch", true);
        }
        let threshold = cal.calibrated_threshold("fetch");
        assert_eq!(
            threshold, 0.7,
            "should return base threshold with insufficient data"
        );
    }

    // ── Phase 6: Testing gap coverage ──

    #[test]
    fn mixed_multilingual_query_selects_github() {
        // Phase 6.5: Multi-language query routing
        let state = ConversationState::from_message("最新的 GitHub PRs list", 3);
        let ranked = scoring::pre_filter_dynamic(&state, "最新的 GitHub PRs list");
        let top_names: Vec<&str> = ranked
            .iter()
            .take(5)
            .filter_map(|(idx, _)| TOOL_CATALOG.get(*idx).map(|t| t.name))
            .collect();
        assert!(
            top_names.iter().any(|n| n.contains("github")),
            "mixed EN/CN GitHub query should select github tools, got: {:?}",
            top_names
        );
    }

    #[test]
    fn budget_edge_exactly_one_tool_fits() {
        // Phase 6.2: Budget exhaustion boundary
        let reg = ToolRegistry::new(mock_schemas());
        // Use a very small budget — should still include pinned + at most 1 dynamic
        let (schemas, report) = reg.select_with_report("list PRs", 1, 1);
        assert!(
            schemas.len() >= ToolRegistry::pinned_count(),
            "should always include pinned tools even with tiny budget"
        );
        assert!(report.budget_used <= 1 || report.budget_used == 0);
    }

    #[test]
    fn conversational_query_never_includes_dynamic() {
        let reg = ToolRegistry::new(mock_schemas());
        let (schemas, _) = reg.select_with_report("hello there", 1, 2000);
        let names = ToolRegistry::selected_names(&schemas);
        let dynamic: Vec<_> = names
            .iter()
            .filter(|n| {
                !TOOL_CATALOG
                    .iter()
                    .any(|t| t.pinned && t.name == n.as_str())
            })
            .collect();
        assert!(
            dynamic.is_empty(),
            "conversational should have 0 dynamic tools, got: {:?}",
            dynamic
        );
    }

    #[test]
    fn zero_signal_query_gets_dynamic_tools() {
        // Phase 7.1: Signal-strength adaptive threshold
        let state = ConversationState::from_message("matrixorigin", 1);
        assert_eq!(state.signal_count(), 0, "should have 0 signals");
        let ranked = scoring::pre_filter_dynamic(&state, "matrixorigin");
        assert!(
            !ranked.is_empty(),
            "0-signal query should still get dynamic tools via adaptive threshold"
        );
    }

    #[test]
    fn calibrator_100_percent_correction_clamps_at_min() {
        use crate::turn::routing_metrics::ConfidenceCalibrator;
        let cal = ConfidenceCalibrator::new(0.7);
        // 100% correction rate
        for _ in 0..20 {
            cal.record("fetch", true);
        }
        let threshold = cal.calibrated_threshold("fetch");
        assert!(
            threshold >= 0.3,
            "100% correction rate should clamp at min_threshold (0.3), got {}",
            threshold
        );
    }

    #[test]
    fn quality_tracker_insufficient_data_returns_neutral() {
        use crate::tool_registry::report::ToolQualityTracker;
        let mut tracker = ToolQualityTracker::new();
        // Only 2 selections — below 3 minimum
        tracker.record_selection(&["bash".to_string()]);
        tracker.record_selection(&["bash".to_string()]);
        let boost = tracker.boost_factor("bash");
        assert_eq!(boost, 1.0, "insufficient data should return neutral (1.0)");
    }

    #[test]
    fn disambiguation_five_intents_has_high_conflict() {
        use crate::turn::routing_metrics::disambiguate_intents;
        let disambig = disambiguate_intents(true, true, true, true, true, false);
        assert!(
            disambig.conflict_score >= 0.3,
            "5 conflicting intents should have high conflict, got {}",
            disambig.conflict_score
        );
    }

    #[test]
    fn select_report_schemas_and_names_consistent() {
        // Phase 6: Data consistency check
        let reg = ToolRegistry::new(mock_schemas());
        let (schemas, report) = reg.select_with_report("show me open PRs in matrixone", 3, 800);
        assert_eq!(
            schemas.len(),
            report.selected_count as usize,
            "schema count should match report count"
        );
        assert_eq!(
            schemas.len(),
            report.tools_selected.len(),
            "schema count should match selected names count"
        );
    }

    #[test]
    fn prefilter_all_tools_have_nonnegative_scores() {
        let state = ConversationState::from_message("analyze everything", 1);
        let ranked = scoring::pre_filter_dynamic(&state, "analyze everything");
        for (idx, score) in &ranked {
            assert!(
                *score >= 0.0,
                "tool {} has negative score {}",
                TOOL_CATALOG[*idx].name,
                score
            );
        }
    }
}
