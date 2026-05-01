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
        word_boundary_match(&haystack.to_lowercase(), needle)
    }

    #[test]
    fn plural_s() {
        let cases = [
            ("show commits", "commit"),
            ("list branches", "branch"),
            ("recent merges", "merge"),
            ("available tools", "tool"),
            ("changed files", "file"),
            ("open prs", "pr"),
        ];
        for (haystack, needle) in &cases {
            assert!(
                wbm(haystack, needle),
                "'{haystack}' should match '{needle}'"
            );
        }
    }

    #[test]
    fn plural_es() {
        let cases = [
            ("open issues", "issue"),
            ("remote branches", "branch"),
            ("recent fixes", "fix"),
        ];
        for (haystack, needle) in &cases {
            assert!(
                wbm(haystack, needle),
                "'{haystack}' should match '{needle}'"
            );
        }
    }

    #[test]
    fn past_tense_ed() {
        let cases = [
            ("committed yesterday", "commit"),
            ("merged into main", "merge"),
            ("rebased on main", "rebase"),
            ("fixed the bug", "fix"),
            ("analyzed the code", "analyze"),
        ];
        for (haystack, needle) in &cases {
            assert!(
                wbm(haystack, needle),
                "'{haystack}' should match '{needle}'"
            );
        }
    }

    #[test]
    fn gerund_ing() {
        let cases = [
            ("committing changes", "commit"),
            ("merging branches", "merge"),
            ("rebasing on main", "rebase"),
            ("debugging the issue", "debug"),
            ("stashing changes", "stash"),
            ("branching strategy", "branch"),
        ];
        for (haystack, needle) in &cases {
            assert!(
                wbm(haystack, needle),
                "'{haystack}' should match '{needle}'"
            );
        }
    }

    #[test]
    fn doubled_consonant_suffix() {
        assert!(wbm("committing now", "commit"));
        assert!(wbm("just committed", "commit"));
    }

    #[test]
    fn multiword_stems() {
        assert!(wbm("show pull requests", "pull request"));
        assert!(wbm("create a pull request", "pull request"));
    }

    #[test]
    fn false_positive_prevention() {
        assert!(!wbm("community guidelines", "commit"));
        assert!(!wbm("mission statement", "miss"));
        assert!(!wbm("this is a test", "hi"));
        assert!(!wbm("tokenbudget", "ok"));
    }

    #[test]
    fn cjk_keywords_unaffected() {
        assert!(wbm("查看仓库信息", "仓库"));
        assert!(wbm("最近的提交", "提交"));
        assert!(wbm("切换分支", "分支"));
    }

    #[test]
    fn exact_match() {
        assert!(wbm("git diff HEAD", "git"));
        assert!(wbm("show the diff", "diff"));
        assert!(wbm("review the pr", "pr"));
        assert!(wbm("check ci status", "ci"));
    }

    #[test]
    fn universality_proof() {
        // Made-up stems prove the RULE works, not keyword lists
        assert!(wbm("the frobnicators are ready", "frobnicator"));
        assert!(wbm("frobulated the data", "frobulate"));
        assert!(wbm("frobulating now", "frobulate"));
        assert!(wbm("runs efficiently", "efficient"));
        assert!(wbm("deployment succeeded", "deploy"));
        assert!(wbm("compilation failed", "compila"));
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
        assert_eq!(detect_divergence(&[]).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn healthy_all_productive() {
        let sigs = make_sigs(&[&["memory_store"], &["github_list_prs"], &["write_file"]]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn healthy_alternating() {
        let sigs = make_sigs(&[&["bash"], &["write_file"], &["bash"], &["memory_store"]]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    // ── Exploring semantics removed under P2.5 ──
    // (Under new signature-diversity assessment, "Exploring" is no longer
    // an injection-triggering state and distinct-sig rounds are Healthy.)

    #[test]
    fn diverse_rounds_are_healthy() {
        let sigs = make_sigs(&[&["write_file"], &["bash"]]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn diverse_three_rounds_still_healthy() {
        let sigs = make_sigs(&[&["write_file"], &["bash"], &["read_file"]]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    // ── Diverging patterns (the bad path) ──
    // P2.5: Diverging fires only on exact signature repetition across the
    // full rolling window, NOT on diverse exploration tools.

    #[test]
    fn diverging_exact_repeat_three_rounds() {
        let sigs = make_sigs(&[&["bash"], &["bash"], &["bash"]]);
        assert!(matches!(
            detect_divergence(&sigs).unwrap(),
            DivergenceStatus::Diverging(_)
        ));
    }

    #[test]
    fn diverging_mixed_tool_names_healthy() {
        // Previously this was "classic Diverging" (all from whitelist).
        // Now: diverse sigs → Healthy (real work).
        let sigs = make_sigs(&[
            &["bash"],
            &["bash"],
            &["list_dir"],
            &["read_file"],
            &["grep"],
        ]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn long_diverse_session_is_healthy() {
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
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn diverging_exact_multi_tool_repeat() {
        // Repeating the SAME sig-set across rounds (genuine loop) → Diverging.
        let sigs = make_sigs(&[&["bash", "grep"], &["bash", "grep"], &["bash", "grep"]]);
        assert!(matches!(
            detect_divergence(&sigs).unwrap(),
            DivergenceStatus::Diverging(_)
        ));
    }

    // ── Reset behavior ──

    #[test]
    fn productive_tool_changes_sig_to_healthy() {
        // After a productive call, the signature set changes; new window
        // becomes diverse → Healthy (no injection).
        let sigs = make_sigs(&[
            &["bash"],
            &["bash"],
            &["bash"],
            &["bash"],
            &["memory_store"],
            &["bash"],
            &["bash"],
        ]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    #[test]
    fn reset_mixed_round_with_productive() {
        let sigs = make_sigs(&[&["bash"], &["bash"], &["bash", "write_file"]]);
        assert_eq!(detect_divergence(&sigs).unwrap(), DivergenceStatus::Healthy);
    }

    // ── Correction prompt ──

    #[test]
    fn correction_prompt_exists_and_is_meaningful() {
        // P2.5 rewrote this as a task-type-agnostic template. Key markers:
        // it identifies a repetition loop and instructs a change in action.
        assert!(
            DIVERGENCE_CORRECTION.contains("same tool calls")
                || DIVERGENCE_CORRECTION.contains("same arguments")
                || DIVERGENCE_CORRECTION.contains("progress")
        );
        assert!(
            DIVERGENCE_CORRECTION.len() > 100,
            "Correction prompt should be substantial"
        );
    }

    // ── Turn complete event with divergence ──

    #[test]
    fn turn_complete_event_healthy() {
        let event = astra_runtime::build_turn_complete_event(
            true,
            false,
            &DivergenceStatus::Healthy,
            None,
            None,
        );
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
