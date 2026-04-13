//! Phase 8 Regression Tests: Universal Stemming, Recall-First Selection, Divergence Detection
//!
//! Three systemic principles enforced by regression:
//!
//! 1. **Universal rules**: All keyword matching uses stem-based rules, NOT per-keyword overrides.
//!    "commits" matches "commit" by the SAME rule that "issues" matches "issue".
//!
//! 2. **Recall-first**: Non-conversational queries always get ≥ MIN_RECALL_TOOLS dynamic tools.
//!    The system maximizes recall; the LLM handles precision.
//!
//! 3. **Divergence suppression**: Consecutive exploration-only rounds are detected and corrected.
//!    Prevents bash→find→list_dir→read_file token waste loops.

use std::collections::BTreeSet;

use astra_runtime::{
    DIVERGENCE_CORRECTION, DivergenceStatus, detect_divergence,
    tool_registry::{IntentType, TOOL_CATALOG},
};

// Re-export internal functions for testing
use astra_runtime::tool_registry::state::word_boundary_match;

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 1: Universal Stemming — ALL matches by RULE, not by keyword
// ═══════════════════════════════════════════════════════════════════════════════

mod universal_stemming {
    use super::*;

    fn wbm(haystack: &str, needle: &str) -> bool {
        let lower = haystack.to_lowercase();
        let chars: Vec<char> = lower.chars().collect();
        word_boundary_match(&lower, &chars, needle)
    }

    // ── Plural -s ──

    #[test]
    fn plural_s_commits() {
        assert!(wbm("show commits", "commit"));
    }

    #[test]
    fn plural_s_branches() {
        assert!(wbm("list branches", "branch"));
    }

    #[test]
    fn plural_s_merges() {
        assert!(wbm("recent merges", "merge"));
    }

    #[test]
    fn plural_s_tools() {
        assert!(wbm("available tools", "tool"));
    }

    #[test]
    fn plural_s_files() {
        assert!(wbm("changed files", "file"));
    }

    #[test]
    fn plural_s_prs() {
        assert!(wbm("open prs", "pr"));
    }

    // ── Plural -es ──

    #[test]
    fn plural_es_issues() {
        assert!(wbm("open issues", "issue"));
    }

    #[test]
    fn plural_es_branches_git() {
        assert!(wbm("remote branches", "branch"));
    }

    #[test]
    fn plural_es_fixes() {
        assert!(wbm("recent fixes", "fix"));
    }

    // ── Past tense -ed ──

    #[test]
    fn past_committed() {
        assert!(wbm("committed yesterday", "commit"));
    }

    #[test]
    fn past_merged() {
        assert!(wbm("merged into main", "merge"));
    }

    #[test]
    fn past_rebased() {
        assert!(wbm("rebased on main", "rebase"));
    }

    #[test]
    fn past_fixed() {
        assert!(wbm("fixed the bug", "fix"));
    }

    #[test]
    fn past_analyzed() {
        assert!(wbm("analyzed the code", "analyze"));
    }

    // ── Gerund -ing ──

    #[test]
    fn gerund_committing() {
        assert!(wbm("committing changes", "commit"));
    }

    #[test]
    fn gerund_merging() {
        assert!(wbm("merging branches", "merge"));
    }

    #[test]
    fn gerund_rebasing() {
        assert!(wbm("rebasing on main", "rebase"));
    }

    #[test]
    fn gerund_debugging() {
        assert!(wbm("debugging the issue", "debug"));
    }

    // ── Doubled consonant + suffix ──

    #[test]
    fn doubled_committing() {
        assert!(wbm("committing now", "commit"));
    }

    #[test]
    fn doubled_committed() {
        assert!(wbm("just committed", "commit"));
    }

    #[test]
    fn doubled_stashing() {
        // "stash" + "ing" = "stashing" (no doubling)
        assert!(wbm("stashing changes", "stash"));
    }

    // ── Multi-word stems ──

    #[test]
    fn multiword_pull_requests() {
        assert!(wbm("show pull requests", "pull request"));
    }

    #[test]
    fn multiword_pull_request_singular() {
        assert!(wbm("create a pull request", "pull request"));
    }

    // ── False positive prevention ──

    #[test]
    fn no_false_positive_community() {
        assert!(!wbm("community guidelines", "commit"));
    }

    #[test]
    fn no_false_positive_mission() {
        assert!(!wbm("mission statement", "miss"));
    }

    #[test]
    fn no_false_positive_branch_in_branching() {
        // "branching" SHOULD match "branch" — it's a valid stem
        assert!(wbm("branching strategy", "branch"));
    }

    #[test]
    fn no_false_positive_this_contains_hi() {
        // "this" should NOT match "hi" (substring trap)
        assert!(!wbm("this is a test", "hi"));
    }

    #[test]
    fn no_false_positive_token_contains_ok() {
        assert!(!wbm("tokenbudget", "ok"));
    }

    // ── CJK keywords unaffected by stemming ──

    #[test]
    fn cjk_github_keyword() {
        assert!(wbm("查看仓库信息", "仓库"));
    }

    #[test]
    fn cjk_commit_keyword() {
        assert!(wbm("最近的提交", "提交"));
    }

    #[test]
    fn cjk_branch_keyword() {
        assert!(wbm("切换分支", "分支"));
    }

    // ── Exact match still works ──

    #[test]
    fn exact_git() {
        assert!(wbm("git diff HEAD", "git"));
    }

    #[test]
    fn exact_diff() {
        assert!(wbm("show the diff", "diff"));
    }

    #[test]
    fn exact_pr() {
        assert!(wbm("review the pr", "pr"));
    }

    #[test]
    fn exact_ci() {
        assert!(wbm("check ci status", "ci"));
    }

    // ── Universality proof: SAME rule handles all suffixes ──
    // These tests prove that no per-keyword hack is needed.
    // If ANY of these fail, the stemming rule is broken — don't add keywords.

    #[test]
    fn universality_any_noun_plural() {
        // Made-up stems — proves the RULE works, not keyword lists
        assert!(wbm("the frobnicators are ready", "frobnicator"));
    }

    #[test]
    fn universality_any_verb_ed() {
        assert!(wbm("frobulated the data", "frobulate"));
    }

    #[test]
    fn universality_any_verb_ing() {
        assert!(wbm("frobulating now", "frobulate"));
    }

    #[test]
    fn universality_suffix_ly() {
        assert!(wbm("runs efficiently", "efficient"));
    }

    #[test]
    fn universality_suffix_ment() {
        assert!(wbm("deployment succeeded", "deploy"));
    }

    #[test]
    fn universality_suffix_tion() {
        assert!(wbm("compilation failed", "compila")); // NOTE: "compila" + "tion" = "compilation"
    }

    #[test]
    fn universality_suffix_er() {
        assert!(wbm("the debugger crashed", "debug"));
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 2: Recall-First Selection — always ≥ MIN_RECALL_TOOLS for real queries
// ═══════════════════════════════════════════════════════════════════════════════

mod recall_first {
    use astra_runtime::tool_registry::{scoring::pre_filter_dynamic, state::ConversationState};

    const MIN_RECALL_TOOLS: usize = 3;

    /// Helper: count how many dynamic tools are returned for a query
    fn dynamic_tool_count(query: &str) -> usize {
        let state = ConversationState::from_message(query, 1);
        pre_filter_dynamic(&state, query).len()
    }

    // ── Minimum recall guarantee ──

    #[test]
    fn recall_github_query() {
        assert!(dynamic_tool_count("show me the pull requests") >= MIN_RECALL_TOOLS);
    }

    #[test]
    fn recall_git_query() {
        assert!(dynamic_tool_count("git diff HEAD~3") >= MIN_RECALL_TOOLS);
    }

    #[test]
    fn recall_analytical_query() {
        assert!(dynamic_tool_count("why is the build failing") >= MIN_RECALL_TOOLS);
    }

    #[test]
    fn recall_vague_query() {
        // Even vague queries should get tools — LLM decides
        assert!(dynamic_tool_count("matrixorigin") >= MIN_RECALL_TOOLS);
    }

    #[test]
    fn recall_chinese_query() {
        assert!(dynamic_tool_count("查看最近的提交") >= MIN_RECALL_TOOLS);
    }

    #[test]
    fn recall_zero_signal_query() {
        // The exact failure case: "我关注matrixorigin" had 0 signals
        assert!(dynamic_tool_count("我关注matrixorigin") >= MIN_RECALL_TOOLS);
    }

    // ── Conversational queries are still exempt ──

    #[test]
    fn recall_conversational_exempt() {
        // Pure greetings should NOT get dynamic tools
        assert_eq!(dynamic_tool_count("hi"), 0);
    }

    #[test]
    fn recall_thanks_exempt() {
        assert_eq!(dynamic_tool_count("thanks"), 0);
    }

    // ── Intent diversity ──

    #[test]
    fn diversity_github_intent_present() {
        use super::*;
        let state = astra_runtime::tool_registry::state::ConversationState::from_message(
            "show me the github issues",
            1,
        );
        let results = astra_runtime::tool_registry::scoring::pre_filter_dynamic(
            &state,
            "show me the github issues",
        );
        let has_github = results
            .iter()
            .any(|&(idx, _)| TOOL_CATALOG[idx].intents.contains(&IntentType::GitHub));
        assert!(
            has_github,
            "GitHub query must include at least 1 GitHub tool"
        );
    }

    #[test]
    fn diversity_git_intent_present() {
        use super::*;
        let state = astra_runtime::tool_registry::state::ConversationState::from_message(
            "show me the git diff",
            1,
        );
        let results = astra_runtime::tool_registry::scoring::pre_filter_dynamic(
            &state,
            "show me the git diff",
        );
        let has_git = results
            .iter()
            .any(|&(idx, _)| TOOL_CATALOG[idx].intents.contains(&IntentType::Git));
        assert!(has_git, "Git query must include at least 1 Git tool");
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 3: Divergence Detection — catch and correct exploration loops
// ═══════════════════════════════════════════════════════════════════════════════

mod divergence_detection {
    use super::*;

    fn make_sigs(rounds: &[&[&str]]) -> Vec<BTreeSet<String>> {
        rounds
            .iter()
            .map(|tools| tools.iter().map(|t| format!("{}:{{}}", t)).collect())
            .collect()
    }

    // ── Healthy patterns ──

    #[test]
    fn healthy_empty() {
        assert_eq!(detect_divergence(&[]), DivergenceStatus::Healthy);
    }

    #[test]
    fn healthy_all_productive() {
        let sigs = make_sigs(&[&["memory_store"], &["github_list_prs"], &["write_file"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Healthy);
    }

    #[test]
    fn healthy_alternating() {
        let sigs = make_sigs(&[&["bash"], &["write_file"], &["bash"], &["memory_store"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Healthy);
    }

    // ── Exploring patterns ──

    #[test]
    fn exploring_one_round() {
        let sigs = make_sigs(&[&["write_file"], &["bash"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Exploring(1));
    }

    #[test]
    fn exploring_two_rounds() {
        // With MAX_EXPLORATION_ROUNDS=3, two consecutive exploration-only tail rounds → Exploring(2)
        let sigs = make_sigs(&[&["write_file"], &["bash"], &["read_file"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Exploring(2));
    }

    // ── Diverging patterns (the bad path) ──

    #[test]
    fn diverging_three_rounds() {
        // 3 consecutive exploration rounds → Diverging(3) at default MAX_EXPLORATION_ROUNDS
        let sigs = make_sigs(&[&["bash"], &["list_dir"], &["read_file"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(3));
    }

    #[test]
    fn diverging_classic_pattern() {
        // 5 exploration rounds → Diverging(5), hits threshold
        let sigs = make_sigs(&[
            &["bash"],
            &["bash"],
            &["list_dir"],
            &["read_file"],
            &["grep"],
        ]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(5));
    }

    #[test]
    fn diverging_at_threshold() {
        // 8 exploration rounds → Diverging(8), well past threshold
        let sigs = make_sigs(&[
            &["bash"],
            &["list_dir"],
            &["read_file"],
            &["grep"],
            &["glob"],
            &["bash"],
            &["list_dir"],
            &["read_file"],
        ]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(8));
    }

    #[test]
    fn diverging_multi_exploration_per_round() {
        // 3 consecutive exploration-only rounds (multi-tool counts as one round) → Diverging(3)
        let sigs = make_sigs(&[
            &["bash", "grep"],
            &["list_dir", "glob"],
            &["read_file", "bash"],
        ]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Diverging(3));
    }

    // ── Reset behavior ──

    #[test]
    fn reset_by_productive_tool() {
        // Deep divergence resets when a productive tool is used;
        // 2 exploration rounds after reset → Exploring(2) (below MAX_EXPLORATION_ROUNDS=3)
        let sigs = make_sigs(&[
            &["bash"],
            &["bash"],
            &["bash"],
            &["bash"],         // 4 exploration
            &["memory_store"], // productive → reset
            &["bash"],
            &["bash"], // 2 more exploration → Exploring(2)
        ]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Exploring(2));
    }

    #[test]
    fn reset_mixed_round_with_productive() {
        // A round with BOTH exploration and productive tools is NOT exploration-only
        let sigs = make_sigs(&[&["bash"], &["bash"], &["bash", "write_file"]]);
        assert_eq!(detect_divergence(&sigs), DivergenceStatus::Healthy);
    }

    // ── Correction prompt ──

    #[test]
    fn correction_prompt_exists_and_is_meaningful() {
        assert!(DIVERGENCE_CORRECTION.contains("exploring"));
        assert!(DIVERGENCE_CORRECTION.contains("STOP"));
        assert!(
            DIVERGENCE_CORRECTION.len() > 100,
            "Correction prompt should be substantial"
        );
    }

    // ── Turn complete event with divergence ──

    #[test]
    fn turn_complete_event_healthy() {
        let event =
            astra_runtime::build_turn_complete_event(true, false, &DivergenceStatus::Healthy, None);
        assert_eq!(event.get("has_tool_calls").unwrap(), true);
        assert!(event.get("divergence_detected").is_none());
    }

    #[test]
    fn turn_complete_event_diverging_stops_tool_calls() {
        let event = astra_runtime::build_turn_complete_event(
            true,
            false,
            &DivergenceStatus::Diverging(4),
            None,
        );
        // Divergence should force has_tool_calls to false
        assert_eq!(event.get("has_tool_calls").unwrap(), false);
        assert_eq!(event.get("divergence_detected").unwrap(), true);
        assert_eq!(event.get("exploration_rounds").unwrap(), 4);
    }

    #[test]
    fn turn_complete_event_exploring_does_not_stop() {
        let event = astra_runtime::build_turn_complete_event(
            true,
            false,
            &DivergenceStatus::Exploring(2),
            None,
        );
        // Exploring does NOT stop tool calls — only Diverging does
        assert_eq!(event.get("has_tool_calls").unwrap(), true);
        assert!(event.get("divergence_detected").is_none());
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 4: Invariants — properties that must ALWAYS hold
// ═══════════════════════════════════════════════════════════════════════════════

mod invariants {
    use astra_runtime::tool_registry::{scoring::pre_filter_dynamic, state::ConversationState};

    /// INVARIANT: Pinned tools are never in the dynamic selection.
    /// They're always included separately.
    #[test]
    fn pinned_tools_never_in_dynamic_results() {
        let queries = vec![
            "show me pull requests",
            "git diff HEAD",
            "我关注matrixorigin",
            "analyze the code",
            "list all files",
        ];
        for q in queries {
            let state = ConversationState::from_message(q, 1);
            let results = pre_filter_dynamic(&state, q);
            for &(idx, _) in &results {
                assert!(
                    !astra_runtime::tool_registry::TOOL_CATALOG[idx].pinned,
                    "Dynamic results for '{}' should not contain pinned tool '{}'",
                    q,
                    astra_runtime::tool_registry::TOOL_CATALOG[idx].name,
                );
            }
        }
    }

    /// INVARIANT: Scores are in [0.0, 1.0] range.
    #[test]
    fn scores_bounded() {
        let queries = vec![
            "show me pull requests",
            "git diff HEAD",
            "我关注matrixorigin",
            "analyze why the build failed",
        ];
        for q in queries {
            let state = ConversationState::from_message(q, 1);
            let results = pre_filter_dynamic(&state, q);
            for &(_, score) in &results {
                assert!(
                    (0.0..=1.0).contains(&score),
                    "Score {} out of bounds for query '{}'",
                    score,
                    q
                );
            }
        }
    }

    /// INVARIANT: Results are sorted by descending score.
    #[test]
    fn results_sorted_descending() {
        let queries = vec![
            "show me the github pull requests",
            "git diff HEAD~3",
            "analyze the codebase",
        ];
        for q in queries {
            let state = ConversationState::from_message(q, 1);
            let results = pre_filter_dynamic(&state, q);
            for pair in results.windows(2) {
                assert!(
                    pair[0].1 >= pair[1].1,
                    "Results not sorted for '{}': {:.4} < {:.4}",
                    q,
                    pair[0].1,
                    pair[1].1,
                );
            }
        }
    }
}
