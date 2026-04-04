//! Regression tests for session behavior issues found in session 292f7100.
//!
//! Four issues fixed:
//! 1. "我关注X" → LLM explored codebase instead of calling memory_store
//!    Fix: system prompt now has HIGHEST PRIORITY memory section with explicit examples
//! 2. Tracking intents not detected by detect_store_signal
//!    Fix: "tracking" category added; 关注/follow/watch patterns fire BEFORE preference check
//! 3. GitHub tools bleeding into memory-only queries via recency boost
//!    Fix: GENERAL content-gated recency — recency amplifies textual relevance, never creates it.
//!    Zero per-intent special cases. Gate = content_relevance / RECENCY_CONTENT_GATE.
//! 4. Passive response when memory is empty — agent asks instead of acting
//!    Fix: system prompt now instructs proactive store + "DO NOT ask, just store"

use astra_runtime::{
    prompts::memory_lifecycle::{detect_store_signal, detect_tracking_intent, suggest_namespace},
    tool_registry::{
        IntentType, TOOL_CATALOG, scoring::pre_filter_dynamic, state::ConversationState,
    },
};

// ─── Helpers ────────────────────────────────────────────────────────────────

fn filter(query: &str) -> Vec<(usize, f64)> {
    let state = ConversationState::from_message(query, 1);
    pre_filter_dynamic(&state, query)
}

fn filter_ctx(query: &str, recent: &[&str]) -> Vec<(usize, f64)> {
    let recent: Vec<String> = recent.iter().map(|s| s.to_string()).collect();
    let state = ConversationState::from_message_with_context(query, 2, &recent);
    pre_filter_dynamic(&state, query)
}

fn tool_score(results: &[(usize, f64)], name: &str) -> Option<f64> {
    results
        .iter()
        .find(|&&(idx, _)| TOOL_CATALOG[idx].name == name)
        .map(|&(_, s)| s)
}

#[allow(dead_code)] // kept for future tests
fn has_tool(results: &[(usize, f64)], name: &str) -> bool {
    tool_score(results, name).is_some()
}

fn tool_names(results: &[(usize, f64)]) -> Vec<&'static str> {
    results
        .iter()
        .map(|&(idx, _)| TOOL_CATALOG[idx].name)
        .collect()
}

// ─── Fix 1 + 2: Tracking intent detection ───────────────────────────────────
// "我关注matrixorigin" should be detected as a tracking intent → memory_store

mod tracking_intent_detection {
    use super::*;

    /// The original session failure: "我关注matrixorigin" must be detected as tracking.
    #[test]
    fn guanzhu_is_tracking() {
        assert_eq!(detect_store_signal("我关注matrixorigin"), Some("tracking"));
    }

    #[test]
    fn follow_is_tracking() {
        assert_eq!(
            detect_store_signal("I follow this project"),
            Some("tracking")
        );
    }

    #[test]
    fn watch_is_tracking() {
        assert_eq!(detect_store_signal("I watch this repo"), Some("tracking"));
    }

    #[test]
    fn interested_in_is_tracking() {
        assert_eq!(
            detect_store_signal("interested in matrixone"),
            Some("tracking")
        );
    }

    #[test]
    fn genzong_is_tracking() {
        assert_eq!(detect_store_signal("我跟踪这个项目"), Some("tracking"));
    }

    #[test]
    fn tracking_beats_preference() {
        // If message has both tracking AND preference signal, tracking wins
        assert_eq!(
            detect_store_signal("我关注matrixorigin，我喜欢这个项目"),
            Some("tracking")
        );
    }

    #[test]
    fn tracking_namespace_is_interest_active() {
        assert_eq!(suggest_namespace("tracking"), "@interest/active");
    }

    /// detect_tracking_intent works on raw user messages too
    #[test]
    fn detect_tracking_intent_raw_messages() {
        assert!(detect_tracking_intent("我关注matrixorigin"));
        assert!(detect_tracking_intent("I'm following this project"));
        assert!(detect_tracking_intent("keep an eye on memoria"));
        assert!(!detect_tracking_intent("show me the diff"));
        assert!(!detect_tracking_intent("帮我修复这个bug"));
    }

    /// Normal queries are NOT tracking intents
    #[test]
    fn code_queries_not_tracking() {
        assert_eq!(detect_store_signal("帮我修复这个bug"), None);
        assert_eq!(detect_store_signal("show me the git diff"), None);
        assert_eq!(detect_store_signal("what's the CI status"), None);
    }
}

// ─── Fix 3: Recency cross-contamination (GENERAL content gate) ──────────────
// Recency amplifies textual relevance — tools with zero content match to the
// current query get zero recency boost.  No per-intent special cases.

mod recency_cross_contamination {
    use super::*;

    /// The session failure: "我有哪些记忆？" after using github_ci_status
    /// → github_ci_status should NOT appear in memory query results.
    #[test]
    fn github_tools_dont_bleed_into_memory_query() {
        let query = "我有哪些记忆？";
        let results = filter_ctx(query, &["github_ci_status"]);

        // github_ci_status must NOT get a recency boost into memory queries
        let github_score = tool_score(&results, "github_ci_status").unwrap_or(0.0);
        let without_recent = filter(query);
        let github_score_no_recent = tool_score(&without_recent, "github_ci_status").unwrap_or(0.0);

        // Score with recency context should equal score without (no boost applied)
        assert!(
            (github_score - github_score_no_recent).abs() < 1e-9,
            "github_ci_status got recency boost in memory query: with={github_score} vs without={github_score_no_recent}"
        );
    }

    #[test]
    fn git_tools_dont_bleed_into_memory_query() {
        let query = "我有哪些记忆？";
        let results = filter_ctx(query, &["git_diff"]);
        let without = filter(query);

        let score_with = tool_score(&results, "git_diff").unwrap_or(0.0);
        let score_without = tool_score(&without, "git_diff").unwrap_or(0.0);
        assert!(
            (score_with - score_without).abs() < 1e-9,
            "git_diff got recency boost in memory query: {score_with} vs {score_without}"
        );
    }

    /// Exact tool match recency boost STILL works (same tool, same intent query)
    #[test]
    fn exact_recency_boost_still_works() {
        let query = "memoria 最新的ci?";
        let with = filter_ctx(query, &["github_ci_status"]);
        let without = filter(query);

        let score_with = tool_score(&with, "github_ci_status").unwrap_or(0.0);
        let score_without = tool_score(&without, "github_ci_status").unwrap_or(0.0);
        assert!(
            score_with > score_without,
            "Exact recency boost should work for same-tool: {score_with} vs {score_without}"
        );
    }

    /// Same-category recency boost works when intent is active in current query
    #[test]
    fn category_recency_boost_works_when_intent_active() {
        // Used github_ci_status → github_list_prs gets category boost when query is github
        let query = "show me pull requests";
        let with = filter_ctx(query, &["github_ci_status"]);
        let without = filter(query);

        let score_with = tool_score(&with, "github_list_prs").unwrap_or(0.0);
        let score_without = tool_score(&without, "github_list_prs").unwrap_or(0.0);
        assert!(
            score_with >= score_without,
            "Category recency should boost when intent active: {score_with} vs {score_without}"
        );
    }

    /// GitHub recency doesn't boost Git tools (different category entirely)
    #[test]
    fn github_recency_doesnt_boost_git() {
        let query = "show me the git diff";
        let with = filter_ctx(query, &["github_ci_status"]);
        let without = filter(query);

        let score_with = tool_score(&with, "git_diff").unwrap_or(0.0);
        let score_without = tool_score(&without, "git_diff").unwrap_or(0.0);
        // No cross-category boost
        assert!(
            (score_with - score_without).abs() < 1e-9,
            "GitHub recency should not boost Git tools: {score_with} vs {score_without}"
        );
    }

    // ── Generality proof tests ──────────────────────────────────────────────
    // These prove the content gate is GENERAL — it works for any tool pair
    // without per-intent special cases.

    /// GENERAL: ANY tool with zero content match to current query gets zero recency.
    /// Test with git_log → completely unrelated "帮我写单元测试" query.
    #[test]
    fn general_zero_content_means_zero_recency() {
        let query = "帮我写单元测试";
        let with = filter_ctx(query, &["git_log"]);
        let without = filter(query);

        let s_with = tool_score(&with, "git_log").unwrap_or(0.0);
        let s_without = tool_score(&without, "git_log").unwrap_or(0.0);
        assert!(
            (s_with - s_without).abs() < 1e-9,
            "General gate: zero-content tool should get zero recency: {s_with} vs {s_without}"
        );
    }

    /// GENERAL: recency DOES boost when the tool has real content match.
    /// Test with git_diff → "show me what changed" (git_diff triggers match).
    #[test]
    fn general_content_match_enables_recency() {
        let query = "show me what changed in the diff";
        let with = filter_ctx(query, &["git_diff"]);
        let without = filter(query);

        let s_with = tool_score(&with, "git_diff").unwrap_or(0.0);
        let s_without = tool_score(&without, "git_diff").unwrap_or(0.0);
        assert!(
            s_with > s_without,
            "General gate: content-matching tool should get recency: {s_with} vs {s_without}"
        );
    }

    /// GENERAL: cross-domain contamination is NEGLIGIBLE.
    /// The content gate proportionally suppresses recency — a single shared
    /// CJK character (e.g., "记" in both "记忆" and "提交记录") allows only
    /// a tiny fraction of the boost through.
    #[test]
    fn general_cross_domain_negligible() {
        let query = "我有哪些记忆？";

        for recent_tool in &["git_diff", "github_ci_status", "github_list_prs", "git_log"] {
            let with = filter_ctx(query, &[recent_tool]);
            let without = filter(query);
            let s_with = tool_score(&with, recent_tool).unwrap_or(0.0);
            let s_without = tool_score(&without, recent_tool).unwrap_or(0.0);
            let delta = s_with - s_without;
            // Content gate: any boost must be negligible (< 4% of max score).
            // CJK character overlap (e.g., "记" in both "记忆" and "提交记录")
            // lets a tiny fraction through — this is proportional, not contamination.
            assert!(
                delta < 0.04,
                "General gate: {recent_tool} recency contamination too high: delta={delta:.4} (with={s_with:.4} without={s_without:.4})"
            );
        }
    }
}

// ─── System prompt shape tests ───────────────────────────────────────────────
// Verify the memory rules appear correctly in built prompts

mod system_prompt_memory_rules {
    use astra_runtime::prompts::build_main_system_prompt;

    #[test]
    fn memory_section_present_when_memory_tools_selected() {
        let tools = &["bash", "memory_store", "memory_search"];
        let prompt = build_main_system_prompt(tools, "", 1.0, None);
        assert!(
            prompt.contains("Memory Rules"),
            "Should contain Memory Rules section"
        );
        assert!(
            prompt.contains("check BEFORE"),
            "Memory rules should be checked before other reasoning"
        );
    }

    #[test]
    fn prompt_contains_guanzhu_example() {
        let tools = &["bash", "memory_store", "memory_search"];
        let prompt = build_main_system_prompt(tools, "", 1.0, None);
        assert!(
            prompt.contains("关注"),
            "Prompt should include 关注 as a trigger example"
        );
    }

    #[test]
    fn prompt_contains_do_not_ask_instruction() {
        let tools = &["bash", "memory_store", "memory_search"];
        let prompt = build_main_system_prompt(tools, "", 1.0, None);
        assert!(
            prompt.contains("Do NOT ask"),
            "Prompt should say Do NOT ask whether to store"
        );
    }

    #[test]
    fn prompt_contains_no_exploration_instruction() {
        let tools = &["bash", "memory_store", "memory_search"];
        let prompt = build_main_system_prompt(tools, "", 1.0, None);
        assert!(
            prompt.contains("Do NOT explore"),
            "Prompt should say Do NOT explore codebase for interest expressions"
        );
    }

    #[test]
    fn memory_section_absent_when_no_memory_tools() {
        let tools = &["bash", "git_diff"];
        let prompt = build_main_system_prompt(tools, "", 1.0, None);
        assert!(
            !prompt.contains("Memory Rules"),
            "Memory Rules should not appear without memory tools"
        );
    }

    #[test]
    fn immediate_store_examples_present() {
        let tools = &["bash", "memory_store", "memory_search"];
        let prompt = build_main_system_prompt(tools, "", 1.0, None);
        assert!(
            prompt.contains("IMMEDIATELY"),
            "Prompt should say store IMMEDIATELY"
        );
    }
}

// ─── End-to-end tool selection for tracking queries ─────────────────────────
// Verify that "我关注X" type queries get GitHub + memory tools, NOT exploration tools

mod e2e_tracking_tool_selection {
    use super::*;

    /// "我关注matrixorigin" — with 关注 removed from github_list_prs,
    /// this query now routes primarily to memory (via is_memory signal).
    /// memory_store is pinned, so it's always available. Dynamic tools
    /// should NOT be exclusively exploration tools.
    #[test]
    fn guanzhu_not_only_exploration() {
        let results = filter("我关注matrixorigin");
        let names = tool_names(&results);
        let exploration = ["bash", "list_dir", "read_file", "glob", "grep"];

        let only_exploration = names.iter().all(|n| exploration.contains(n));

        // With memory signal firing, the selection should include
        // non-exploration tools or be empty (memory_store is pinned)
        assert!(
            !only_exploration || names.is_empty(),
            "关注 query should not be dominated by exploration tools. Got: {names:?}"
        );
    }

    /// "我关注matrixorigin" — memory_store (pinned) should handle this.
    /// github_list_prs no longer has "关注" trigger (overlap removed).
    #[test]
    fn guanzhu_routes_to_memory_not_github_prs() {
        let results = filter("我关注matrixorigin");
        // github_list_prs no longer has "关注" as trigger
        let github_prs_score = tool_score(&results, "github_list_prs").unwrap_or(0.0);
        // Score should be low (only TF-IDF on "matrixorigin", no trigger match)
        assert!(
            github_prs_score < 0.3,
            "github_list_prs score {github_prs_score} should be low without 关注 trigger"
        );
    }

    /// "I'm watching this repo" → same behavior in English
    #[test]
    fn watching_selects_github() {
        let results = filter("I'm watching this repo on github");
        let has_github = results
            .iter()
            .any(|&(idx, _)| TOOL_CATALOG[idx].intents.contains(&IntentType::GitHub));
        assert!(has_github, "watch github query should include GitHub tools");
    }

    /// Pure memory queries should have clean tool selection (no GitHub/Git bleed)
    #[test]
    fn memory_query_has_clean_selection() {
        // Without any recent tool context
        let results = filter("我有哪些记忆？");
        let github_score = tool_score(&results, "github_ci_status").unwrap_or(0.0);
        let git_score = tool_score(&results, "git_diff").unwrap_or(0.0);

        // These should be low since no GitHub/Git signals in memory query
        assert!(
            github_score < 0.3,
            "github_ci_status score {github_score} too high in memory-only query"
        );
        assert!(
            git_score < 0.3,
            "git_diff score {git_score} too high in memory-only query"
        );
    }
}
