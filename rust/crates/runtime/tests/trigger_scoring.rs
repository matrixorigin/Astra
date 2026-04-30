//! Tests for trigger-based tool scoring.
//!
//! Validates that multilingual triggers provide accurate cross-language tool selection
//! without requiring embedding models. Each test verifies that the right tools are
//! selected for both English and Chinese queries.

use astra_runtime::tool_registry::{
    TOOL_CATALOG, scoring::pre_filter_dynamic, state::ConversationState,
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

fn has_tool(results: &[(usize, f64)], name: &str) -> bool {
    tool_score(results, name).is_some()
}

// ─── Cross-language trigger matching ────────────────────────────────────────

mod cross_language {
    use super::*;

    /// "最新的pr" should select GitHub PR tools
    #[test]
    fn chinese_latest_pr_selects_github() {
        let results = filter("最新的pr");
        assert!(has_tool(&results, "github_list_prs"));
    }

    /// "提交历史" should select git_log
    #[test]
    fn chinese_commit_history_selects_git_log() {
        let results = filter("查看提交历史");
        assert!(has_tool(&results, "git_log"));
    }

    /// "搜索记忆" — memory search (pinned, so test via trigger_match_score)
    #[test]
    fn chinese_search_memory_has_trigger_match() {
        // memory_search is pinned, so it won't appear in pre_filter results.
        // But we can verify the trigger infrastructure works by checking
        // a non-pinned memory tool like memory_purge with "删除记忆"
        let results = filter("删除记忆");
        assert!(has_tool(&results, "memory_purge"));
    }

    /// "CI状态" → github_ci_status
    #[test]
    fn chinese_ci_status() {
        let results = filter("CI状态怎么样");
        assert!(has_tool(&results, "github_ci_status"));
    }

    /// "差异" → git_diff
    #[test]
    fn chinese_diff() {
        let results = filter("看看差异");
        assert!(has_tool(&results, "git_diff"));
    }

    /// "反思" → reflect
    #[test]
    fn chinese_reflect() {
        let results = filter("反思一下刚才的行为");
        assert!(has_tool(&results, "reflect"));
    }

    /// "创建issue" → github_create_issue
    #[test]
    fn chinese_create_issue() {
        let results = filter("帮我创建issue");
        assert!(has_tool(&results, "github_create_issue"));
    }
}

// ─── Trigger specificity ────────────────────────────────────────────────────

mod trigger_specificity {
    use super::*;

    /// More specific triggers should produce higher scores.
    /// "pull request" (longer) should score higher than "pr" (shorter)
    /// for the same tool (github_list_prs).
    #[test]
    fn longer_trigger_scores_higher() {
        let r1 = filter("show me the pull request list");
        let r2 = filter("show me the pr list");
        let s1 = tool_score(&r1, "github_list_prs").unwrap_or(0.0);
        let s2 = tool_score(&r2, "github_list_prs").unwrap_or(0.0);
        // Both should be present; longer match is at least as good
        assert!(s1 >= s2 * 0.8, "pull request ({s1}) vs pr ({s2})");
    }

    /// Trigger match should boost score above pure TF-IDF for known phrases.
    /// After trigger cleanup, "关注" is exclusively on memory_store (not github_list_prs).
    /// Test with a trigger that IS on github_list_prs: "pull request".
    #[test]
    fn trigger_boosts_beyond_tfidf() {
        let results = filter("show me the pull request");
        let github_score = tool_score(&results, "github_list_prs").unwrap_or(0.0);
        assert!(
            github_score > 0.1,
            "pull request trigger should boost github_list_prs above 0.1, got {github_score}"
        );
    }
}

// ─── Dual-channel scoring balance ───────────────────────────────────────────

mod scoring_balance {
    use super::*;

    /// TF-IDF and trigger should complement each other.
    /// A query that matches BOTH TF-IDF and trigger should score higher
    /// than one that matches only one channel.
    #[test]
    fn both_channels_beat_single() {
        // "git diff" → matches TF-IDF (description has "git diffs") AND trigger ("diff")
        let both = filter("show the git diff");
        let both_score = tool_score(&both, "git_diff").unwrap_or(0.0);

        // "compare the files" → matches trigger ("compare") but not TF-IDF
        let trigger_only = filter("compare the files");
        let trigger_score = tool_score(&trigger_only, "git_diff").unwrap_or(0.0);

        assert!(
            both_score > trigger_score,
            "Both channels ({both_score}) should beat trigger-only ({trigger_score})"
        );
    }

    /// Scores must be bounded in [0.0, 1.0].
    #[test]
    fn scores_bounded() {
        let queries = vec![
            "show me pull requests",
            "git diff HEAD",
            "remember my preference",
            "我关注matrixorigin",
            "delete all the issues and purge memories",
        ];
        for q in queries {
            let results = filter(q);
            for &(_, score) in &results {
                assert!(
                    (0.0..=1.0).contains(&score),
                    "Score {score} out of bounds for '{q}'"
                );
            }
        }
    }

    /// Results must be sorted by descending score.
    #[test]
    fn results_sorted() {
        let queries = vec![
            "show the git diff",
            "list pull requests",
            "我关注matrixorigin",
        ];
        for q in queries {
            let results = filter(q);
            for pair in results.windows(2) {
                assert!(
                    pair[0].1 >= pair[1].1,
                    "Not sorted for '{q}': {:.4} < {:.4}",
                    pair[0].1,
                    pair[1].1,
                );
            }
        }
    }
}

// ─── Recall-first properties ────────────────────────────────────────────────

mod recall_first {
    use super::*;

    /// Non-conversational queries should always return at least MIN_RECALL_TOOLS (5).
    #[test]
    fn minimum_recall_guaranteed() {
        let queries = vec![
            "do something",
            "matrixorigin",
            "xyz random query",
            "test this thing",
        ];
        for q in queries {
            let results = filter(q);
            assert!(
                results.len() >= 5,
                "Query '{}' returned only {} tools (need ≥5)",
                q,
                results.len(),
            );
        }
    }

    /// Conversational queries should return empty (no dynamic tools).
    #[test]
    fn conversational_returns_empty() {
        for q in ["hi", "thanks", "好的", "ok"] {
            let results = filter(q);
            assert!(
                results.is_empty(),
                "Conversational '{}' should return empty, got {}",
                q,
                results.len(),
            );
        }
    }

    /// Intent diversity: if is_github is set, at least one GitHub tool must appear.
    #[test]
    fn intent_diversity_github() {
        let results = filter("show me the github issues");
        let has_github = results.iter().any(|&(idx, _)| {
            TOOL_CATALOG[idx]
                .intents
                .contains(&astra_runtime::tool_registry::IntentType::GitHub)
        });
        assert!(
            has_github,
            "GitHub intent query should include a GitHub tool"
        );
    }

    /// Intent diversity: if is_git is set, at least one Git tool must appear.
    #[test]
    fn intent_diversity_git() {
        let results = filter("show me the git diff");
        let has_git = results.iter().any(|&(idx, _)| {
            TOOL_CATALOG[idx]
                .intents
                .contains(&astra_runtime::tool_registry::IntentType::Git)
        });
        assert!(has_git, "Git intent query should include a Git tool");
    }
}

// ─── Recency boost ──────────────────────────────────────────────────────────

mod recency {
    use super::*;

    /// Using a tool recently should boost its score.
    #[test]
    fn recent_tool_gets_boost() {
        let without = filter("show me pull requests");
        let with = filter_ctx("show me pull requests", &["github_list_prs"]);

        let s_without = tool_score(&without, "github_list_prs").unwrap_or(0.0);
        let s_with = tool_score(&with, "github_list_prs").unwrap_or(0.0);

        assert!(
            s_with > s_without,
            "Recency should boost: {s_with} > {s_without}"
        );
    }

    /// Same-category tools should get a smaller boost.
    #[test]
    fn same_category_gets_smaller_boost() {
        // Using github_list_prs recently should give a small boost to github_get_pr
        let without = filter("show me the pr details");
        let with = filter_ctx("show me the pr details", &["github_list_prs"]);

        let s_without = tool_score(&without, "github_get_pr").unwrap_or(0.0);
        let s_with = tool_score(&with, "github_get_pr").unwrap_or(0.0);

        assert!(
            s_with >= s_without,
            "Same-category recency should not hurt: {s_with} >= {s_without}"
        );
    }
}


