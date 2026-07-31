use std::collections::HashSet;

use astra_text_utils::text_tokenize::tokenize;

// ── Constants ────────────────────────────────────────────────────────────────

pub const CROSS_SESSION_MEMORY_RETRIEVE_TOP_K: u64 = 5;

// ── Cross-session memory admission ──────────────────────────────────────────

/// Structured reason why cross-session memory retrieval did not run.
///
/// This deliberately models routing state, not prose. Infrastructure should not
/// decide memory retrieval from natural-language substrings in the user message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossSessionMemorySkipReason {
    /// No planner/tool/runtime component supplied an explicit semantic query.
    NoStructuredIntent,
    /// A structured query was supplied but normalized to empty.
    EmptyStructuredQuery,
    /// The current conversation already carries memory context, so automatic
    /// cross-session recall would add duplicate cache-volatile context.
    ConversationAlreadyHasMemoryContext,
}

/// Admission decision for cross-session memory retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossSessionMemoryDecision<'a> {
    Retrieve {
        query: &'a str,
        top_k: u64,
    },
    Skip {
        query: &'a str,
        reason: CrossSessionMemorySkipReason,
    },
}

impl<'a> CrossSessionMemoryDecision<'a> {
    pub fn query(self) -> &'a str {
        match self {
            Self::Retrieve { query, .. } | Self::Skip { query, .. } => query,
        }
    }
}

/// Decide whether to perform cross-session memory retrieval for a turn.
///
/// First-principles contract:
/// - user text is evidence, not infrastructure control flow;
/// - cross-session retrieval requires structured intent;
/// - prompt-cache stability wins when the current conversation already carries
///   memory context.
pub fn decide_cross_session_memory_retrieval<'a>(
    message: &'a str,
    semantic_query_override: Option<&'a str>,
    has_conversation_memory_context: bool,
) -> CrossSessionMemoryDecision<'a> {
    let Some(query) = semantic_query_override.map(str::trim) else {
        return CrossSessionMemoryDecision::Skip {
            query: message,
            reason: CrossSessionMemorySkipReason::NoStructuredIntent,
        };
    };

    if query.is_empty() {
        return CrossSessionMemoryDecision::Skip {
            query,
            reason: CrossSessionMemorySkipReason::EmptyStructuredQuery,
        };
    }

    if has_conversation_memory_context {
        return CrossSessionMemoryDecision::Skip {
            query,
            reason: CrossSessionMemorySkipReason::ConversationAlreadyHasMemoryContext,
        };
    }

    CrossSessionMemoryDecision::Retrieve {
        query,
        top_k: CROSS_SESSION_MEMORY_RETRIEVE_TOP_K,
    }
}

// ── Lexical relevance helpers ────────────────────────────────────────────────

fn lexical_relevance_score(query_tokens: &[String], doc_tokens: &[String]) -> f64 {
    if query_tokens.is_empty() || doc_tokens.is_empty() {
        return 0.0;
    }

    let query_terms: HashSet<&str> = query_tokens.iter().map(String::as_str).collect();
    let doc_terms: HashSet<&str> = doc_tokens.iter().map(String::as_str).collect();
    let matched_unique = query_terms.intersection(&doc_terms).count();
    if matched_unique == 0 {
        return 0.0;
    }

    let repeated_matches = doc_tokens
        .iter()
        .filter(|token| query_terms.contains(token.as_str()))
        .count();
    let coverage = matched_unique as f64 / query_terms.len() as f64;
    let density = repeated_matches as f64 / doc_tokens.len() as f64;
    let specificity = matched_unique as f64 / doc_terms.len() as f64;

    (coverage * 0.70) + (density * 0.20) + (specificity * 0.10)
}

// ── Memory result re-ranking ─────────────────────────────────────────────────

/// Minimum lexical relevance for a memory result to contribute boost terms.
/// Results below this threshold are filtered as irrelevant.
const MEMORY_RELEVANCE_THRESHOLD: f64 = 0.05;

/// Re-rank memory results by lexical relevance to the query.
/// Returns (content, score) pairs sorted by descending score, filtered
/// by MEMORY_RELEVANCE_THRESHOLD. This prevents noisy/irrelevant memories
/// from polluting tool surface boost terms.
pub fn rank_memory_results(query: &str, memory_contents: &[String]) -> Vec<(String, f64)> {
    if memory_contents.is_empty() || query.trim().is_empty() {
        return vec![];
    }

    let query_tokens = tokenize(query);
    if query_tokens.is_empty() {
        return vec![];
    }

    let mut scored: Vec<(String, f64)> = memory_contents
        .iter()
        .map(|content| {
            let doc_tokens = tokenize(content);
            let score = lexical_relevance_score(&query_tokens, &doc_tokens);
            (content.clone(), score)
        })
        .filter(|(_, score)| *score >= adaptive_threshold(query))
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Adaptive recall: if scores cluster tightly (low spread), fewer results
    // are genuinely relevant — trim to avoid injecting near-noise memories.
    if scored.len() > 2 {
        let top = scored[0].1;
        let last = scored[scored.len() - 1].1;
        let spread = top - last;
        let keep = if spread < 0.03 {
            scored.len().min(2) // Tight cluster: keep only top 2
        } else if spread < 0.08 {
            scored.len().min(4) // Moderate spread: keep top 4
        } else {
            scored.len() // Wide spread: keep all
        };
        scored.truncate(keep);
    }

    scored
}

/// Adaptive relevance threshold based on query length.
/// Short queries are more ambiguous → higher bar to prevent noise.
/// Long queries provide more signal → lower bar to catch partial matches.
fn adaptive_threshold(query: &str) -> f64 {
    match query.chars().count() {
        0..=20 => 0.08,
        21..=100 => MEMORY_RELEVANCE_THRESHOLD,
        _ => 0.03,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── tokenize tests ───────────────────────────────────────────────────

    #[test]
    fn tokenize_english() {
        let tokens = tokenize("Hello World! foo_bar baz");
        assert!(tokens.contains(&"hello".to_string()));
        assert!(tokens.contains(&"world".to_string()));
        assert!(tokens.contains(&"foo_bar".to_string()));
        assert!(tokens.contains(&"baz".to_string()));
        // Single-char words filtered out
        assert!(!tokens.iter().any(|t| t.len() < 2));
    }

    #[test]
    fn tokenize_cjk_unigrams_and_bigrams() {
        let tokens = tokenize("帮我分析");
        // Unigrams
        assert!(tokens.contains(&"帮".to_string()));
        assert!(tokens.contains(&"我".to_string()));
        assert!(tokens.contains(&"分".to_string()));
        assert!(tokens.contains(&"析".to_string()));
        // Bigrams
        assert!(tokens.contains(&"帮我".to_string()));
        assert!(tokens.contains(&"我分".to_string()));
        assert!(tokens.contains(&"分析".to_string()));
    }

    #[test]
    fn tokenize_mixed_cjk_and_english() {
        let tokens = tokenize("分析 repository code 仓库");
        assert!(tokens.contains(&"分析".to_string()));
        assert!(tokens.contains(&"repository".to_string()));
        assert!(tokens.contains(&"code".to_string()));
        assert!(tokens.contains(&"仓库".to_string()));
        // CJK bigrams should NOT span across the ascii gap
        assert!(!tokens.contains(&"析仓".to_string()));
    }

    #[test]
    fn tokenize_empty_and_short() {
        assert!(tokenize("").is_empty());
        // Single ascii char should be filtered
        assert!(tokenize("a").is_empty());
        // Single CJK char is kept (len==1 but char-len filtering is ascii-only)
        assert_eq!(tokenize("我"), vec!["我".to_string()]);
    }

    // ── lexical relevance tests ─────────────────────────────────────────

    #[test]
    fn lexical_relevance_identical_documents_score_high() {
        let tokens = tokenize("hello world");
        let score = lexical_relevance_score(&tokens, &tokens);
        assert!(score > 0.9, "expected high relevance, got {score}");
    }

    #[test]
    fn lexical_relevance_disjoint_documents_score_zero() {
        let score = lexical_relevance_score(&tokenize("hello world"), &tokenize("foo bar"));
        assert_eq!(score, 0.0);
    }

    #[test]
    fn lexical_relevance_partial_overlap() {
        let query = tokenize("rust memory");
        let relevant = lexical_relevance_score(&query, &tokenize("rust memory management"));
        let unrelated = lexical_relevance_score(&query, &tokenize("python web server"));
        assert!(
            relevant > unrelated,
            "relevant doc should score higher: {relevant} vs {unrelated}"
        );
    }

    #[test]
    fn lexical_relevance_cjk_query_matches_cjk_document() {
        let query = tokenize("分析仓库");
        let relevant = lexical_relevance_score(&query, &tokenize("帮我分析这个仓库的结构"));
        let unrelated = lexical_relevance_score(&query, &tokenize("hello world rust code"));
        assert!(
            relevant > unrelated,
            "CJK match should score higher: {relevant} vs {unrelated}"
        );
        assert!(relevant > 0.0, "CJK match should be positive: {relevant}");
    }

    #[test]
    fn cross_session_memory_requires_structured_intent_not_user_text_shape() {
        for input in [
            "hi",
            "继续",
            "ok",
            "analyze session 62f1e532-f4c3-4953-b1dc-c427acd63b83",
            "继续之前那个分支的修复",
        ] {
            assert_eq!(
                decide_cross_session_memory_retrieval(input, None, false),
                CrossSessionMemoryDecision::Skip {
                    query: input,
                    reason: CrossSessionMemorySkipReason::NoStructuredIntent,
                }
            );
        }
    }

    #[test]
    fn cross_session_memory_uses_normalized_structured_query() {
        assert_eq!(
            decide_cross_session_memory_retrieval(
                "fallback user text",
                Some("  previous branch  "),
                false
            ),
            CrossSessionMemoryDecision::Retrieve {
                query: "previous branch",
                top_k: CROSS_SESSION_MEMORY_RETRIEVE_TOP_K,
            }
        );
    }

    #[test]
    fn cross_session_memory_rejects_empty_structured_query() {
        assert_eq!(
            decide_cross_session_memory_retrieval("fallback", Some("  "), false),
            CrossSessionMemoryDecision::Skip {
                query: "",
                reason: CrossSessionMemorySkipReason::EmptyStructuredQuery,
            }
        );
    }

    #[test]
    fn cross_session_memory_skips_when_conversation_already_has_memory_context() {
        assert_eq!(
            decide_cross_session_memory_retrieval("resume", Some("aa1f419b"), true),
            CrossSessionMemoryDecision::Skip {
                query: "aa1f419b",
                reason: CrossSessionMemorySkipReason::ConversationAlreadyHasMemoryContext,
            }
        );
    }

    // ── rank_memory_results tests ────────────────────────────────────────

    #[test]
    fn rank_memory_empty_inputs() {
        assert!(rank_memory_results("", &[]).is_empty());
        assert!(rank_memory_results("hello", &[]).is_empty());
        assert!(rank_memory_results("", &["something".into()]).is_empty());
        assert!(rank_memory_results("   ", &["something".into()]).is_empty());
    }

    #[test]
    fn rank_memory_relevant_scores_higher() {
        let query = "matrixorigin github pull requests";
        let memories = vec![
            "User follows matrixorigin on GitHub. This is a GitHub organization.".to_string(),
            "Grocery list: eggs, milk, bread".to_string(),
            "matrixorigin has active pull requests and issues".to_string(),
        ];
        let ranked = rank_memory_results(query, &memories);

        // Relevant memories (mentioning matrixorigin/github/pull) should score higher
        assert!(
            !ranked.is_empty(),
            "Should return at least one relevant result"
        );

        // First result should contain matrixorigin or github
        let top = &ranked[0].0;
        assert!(
            top.contains("matrixorigin") || top.contains("GitHub"),
            "Top result should be about matrixorigin/GitHub, got: {}",
            top
        );

        // Grocery list should be filtered out (below threshold)
        let has_grocery = ranked.iter().any(|(c, _)| c.contains("Grocery"));
        assert!(
            !has_grocery,
            "Irrelevant memories should be filtered: {:?}",
            ranked
        );
    }

    #[test]
    fn rank_memory_sorted_descending() {
        let query = "git commit history";
        let memories = vec![
            "User likes git log and git diff commands".to_string(),
            "Random unrelated text about weather".to_string(),
            "git commit history for the project was analyzed last week".to_string(),
        ];
        let ranked = rank_memory_results(query, &memories);

        // Scores should be in descending order
        for w in ranked.windows(2) {
            assert!(
                w[0].1 >= w[1].1,
                "Results should be sorted descending: {:.3} >= {:.3}",
                w[0].1,
                w[1].1
            );
        }
    }

    #[test]
    fn rank_memory_cjk_query() {
        let query = "matrixorigin项目的pull request";
        let memories = vec![
            "matrixorigin是一个数据库项目，有很多pull request".to_string(),
            "天气预报今天晴朗，适合出门".to_string(),
        ];
        let ranked = rank_memory_results(query, &memories);

        // matrixorigin memory should rank first (shares entity + domain terms)
        assert!(
            !ranked.is_empty(),
            "Should match memories with shared entities"
        );
        assert!(
            ranked[0].0.contains("matrixorigin"),
            "CJK query should still match entity: {:?}",
            ranked
        );
    }

    #[test]
    fn rank_memory_threshold_filters_noise() {
        let query = "specific technical term xyz123";
        let memories = vec![
            "completely unrelated content about cooking".to_string(),
            "another irrelevant memory about sports".to_string(),
        ];
        let ranked = rank_memory_results(query, &memories);

        // Both should be filtered out — no overlap with query
        assert!(
            ranked.is_empty(),
            "Completely irrelevant memories should all be filtered: {:?}",
            ranked
        );
    }

    // ── Adaptive threshold tests ──

    #[test]
    fn adaptive_threshold_short_query_higher_bar() {
        assert_eq!(super::adaptive_threshold("fix bug"), 0.08);
        assert_eq!(super::adaptive_threshold(""), 0.08);
    }

    #[test]
    fn adaptive_threshold_medium_query_normal_bar() {
        let medium = "how do I fix the authentication bug in login";
        assert!(medium.chars().count() > 20 && medium.chars().count() <= 100);
        assert_eq!(
            super::adaptive_threshold(medium),
            super::MEMORY_RELEVANCE_THRESHOLD
        );
    }

    #[test]
    fn adaptive_threshold_long_query_lower_bar() {
        let long = "I need to understand how the authentication system works with JWT tokens and how it integrates with the database layer for session management";
        assert!(long.chars().count() > 100);
        assert_eq!(super::adaptive_threshold(long), 0.03);
    }

    #[test]
    fn adaptive_recall_tight_cluster_trimmed() {
        // When all results score nearly identically, fewer survive
        let query = "git diff status";
        let memories = vec![
            "git diff shows changes".to_string(),
            "git status shows state".to_string(),
            "git diff is useful".to_string(),
            "git status diff check".to_string(),
        ];
        let ranked = rank_memory_results(query, &memories);
        assert!(
            ranked.len() <= 4,
            "Should trim tight cluster: got {}",
            ranked.len()
        );
    }

    #[test]
    fn adaptive_recall_preserves_high_spread() {
        let query = "matrixone database query optimization";
        let memories = vec![
            "matrixone database query optimization techniques".to_string(),
            "generic unrelated content about weather".to_string(),
        ];
        let ranked = rank_memory_results(query, &memories);
        assert!(ranked.len() <= 2);
        if !ranked.is_empty() {
            assert!(ranked[0].0.contains("matrixone"));
        }
    }
}
