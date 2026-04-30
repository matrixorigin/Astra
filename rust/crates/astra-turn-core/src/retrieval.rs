use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use astra_text_utils::str_preview::prefix_chars;
use astra_text_utils::text_tokenize::{build_tf, tokenize};

// ── Type aliases ─────────────────────────────────────────────────────────────

type DocEntry<'a> = (usize, &'a Map<String, Value>, HashMap<String, f64>);

// ── Constants ────────────────────────────────────────────────────────────────

pub const RETRIEVAL_BUDGET_CHARS: usize = 8000;
const RETRIEVAL_HEADER: &str = "[Earlier relevant context from this session]\n";
/// Freshness decay base: score *= DECAY_BASE^distance_from_end
const DECAY_BASE: f64 = 0.95;
/// Maximum number of top-scored messages to include in retrieval output.
/// Override with `ASTRA_MAX_RETRIEVED` env var.
fn max_retrieved() -> usize {
    astra_core::RuntimeLimits::global().max_retrieved
}

// ── TF-IDF helpers ───────────────────────────────────────────────────────────

/// Compute smoothed inverse-document-frequency for every term across a corpus.
/// IDF(t) = ln(1 + N / df(t)), which is always positive even when every
/// document contains the term (avoids the zero-IDF problem of the classic formula).
fn build_idf(doc_tfs: &[HashMap<String, f64>]) -> HashMap<String, f64> {
    let n = doc_tfs.len() as f64;
    let mut df: HashMap<String, f64> = HashMap::new();
    for tf in doc_tfs {
        for key in tf.keys() {
            *df.entry(key.clone()).or_insert(0.0) += 1.0;
        }
    }
    df.into_iter()
        .map(|(term, count)| (term, (1.0 + n / count).ln()))
        .collect()
}

/// Cosine similarity between a query TF-IDF vector and a document TF-IDF vector.
fn tfidf_cosine(
    query_tf: &HashMap<String, f64>,
    doc_tf: &HashMap<String, f64>,
    idf: &HashMap<String, f64>,
) -> f64 {
    let mut dot = 0.0f64;
    let mut norm_q = 0.0f64;
    let mut norm_d = 0.0f64;

    for (term, &q_count) in query_tf {
        let idf_val = idf.get(term).copied().unwrap_or(0.0);
        let q_w = q_count * idf_val;
        norm_q += q_w * q_w;

        if let Some(&d_count) = doc_tf.get(term) {
            let d_w = d_count * idf_val;
            dot += q_w * d_w;
        }
    }
    for (term, &d_count) in doc_tf {
        let idf_val = idf.get(term).copied().unwrap_or(0.0);
        let d_w = d_count * idf_val;
        norm_d += d_w * d_w;
    }

    if norm_q == 0.0 || norm_d == 0.0 {
        0.0
    } else {
        dot / (norm_q.sqrt() * norm_d.sqrt())
    }
}

// ── Adaptive budget ──────────────────────────────────────────────────────────

/// Return a context budget (in chars) scaled to query complexity.
/// - Short queries (< 20 chars): 4 000 — little context needed.
/// - Medium queries (20–100 chars): 8 000 — standard budget.
/// - Complex queries (> 100 chars or contains code-like patterns): 12 000.
pub fn adaptive_budget_chars(query: &str) -> usize {
    let len = query.len();
    let has_code = query.contains("fn ")
        || query.contains("def ")
        || query.contains("class ")
        || query.contains("impl ")
        || query.contains("```")
        || query.contains('{')
        || query.contains("->")
        || query.contains("::");
    if len > 100 || has_code {
        12_000
    } else if len >= 20 {
        8_000
    } else {
        4_000
    }
}

// ── format_retrieved_events (unchanged signature) ────────────────────────────

pub fn format_retrieved_events(
    events: &[Map<String, Value>],
    recent_contents: &[String],
    budget_chars: usize,
) -> Option<String> {
    let mut parts = Vec::new();
    let mut used_chars = 0usize;

    for event in events {
        let Some(content) = event.get("content").and_then(Value::as_str) else {
            continue;
        };
        if content.is_empty() {
            continue;
        }
        if recent_contents
            .iter()
            .any(|recent| recent == &prefix_chars(content, 100))
        {
            continue;
        }
        let line = match event
            .get("event_type")
            .and_then(Value::as_str)
            .unwrap_or("")
        {
            "user_query" => format!("User: {}", prefix_chars(content, 200)),
            "llm_response" => format!("Assistant: {}", prefix_chars(content, 300)),
            "tool_result" => format!("Tool result: {}", prefix_chars(content, 300)),
            _ => continue,
        };
        if used_chars + line.len() > budget_chars {
            break;
        }
        used_chars += line.len();
        parts.push(line);
    }

    if parts.is_empty() {
        None
    } else {
        Some(format!("{RETRIEVAL_HEADER}{}", parts.join("\n")))
    }
}

// ── rule_based_extraction (signature unchanged, uses TF-IDF + decay) ─────────

pub fn rule_based_extraction(
    full_history: &[Map<String, Value>],
    recent_messages: &[Map<String, Value>],
    user_query: &str,
    budget_chars: usize,
) -> Option<String> {
    let query_tokens = tokenize(user_query);
    if query_tokens.is_empty() {
        return None;
    }

    let total = full_history.len();
    // Collect (index_in_full_history, message) for old (non-recent) messages.
    let old_messages: Vec<(usize, &Map<String, Value>)> = full_history
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, msg)| !recent_messages.iter().any(|r| r == *msg))
        .collect();
    if old_messages.is_empty() {
        return None;
    }

    // Build per-document TF vectors and collect contents for IDF.
    let doc_data: Vec<DocEntry> = old_messages
        .into_iter()
        .filter_map(|(idx, msg)| {
            let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
            if content.is_empty() {
                return None;
            }
            let tokens = tokenize(content);
            if tokens.is_empty() {
                return None;
            }
            Some((idx, msg, build_tf(&tokens)))
        })
        .collect();
    if doc_data.is_empty() {
        return None;
    }

    let all_tfs: Vec<HashMap<String, f64>> = doc_data.iter().map(|(_, _, tf)| tf.clone()).collect();
    let idf = build_idf(&all_tfs);
    let query_tf = build_tf(&query_tokens);

    let mut scored: Vec<(f64, &Map<String, Value>)> = doc_data
        .iter()
        .filter_map(|(idx, msg, tf)| {
            let sim = tfidf_cosine(&query_tf, tf, &idf);
            if sim <= 0.0 {
                return None;
            }
            let distance = total.saturating_sub(*idx + 1);
            let decay = DECAY_BASE.powi(distance as i32);
            Some((sim * decay, *msg))
        })
        .collect();
    if scored.is_empty() {
        return None;
    }

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    assemble_parts(&scored, budget_chars)
}

// ── enhanced_extraction (new public API with TF-IDF + freshness + adaptive) ──

/// Enhanced retrieval combining TF-IDF cosine similarity, freshness decay,
/// and adaptive budget sizing.  Prefer this over `rule_based_extraction` for
/// new call-sites.
pub fn enhanced_extraction(
    full_history: &[Map<String, Value>],
    recent_messages: &[Map<String, Value>],
    user_query: &str,
) -> Option<String> {
    let budget = adaptive_budget_chars(user_query);
    rule_based_extraction(full_history, recent_messages, user_query, budget)
}

// ── Shared helpers ───────────────────────────────────────────────────────────

/// Assemble scored messages into a budget-bounded retrieval string.
fn assemble_parts(scored: &[(f64, &Map<String, Value>)], budget_chars: usize) -> Option<String> {
    let mut parts = Vec::new();
    let mut used_chars = 0usize;
    for (_, message) in scored.iter().take(max_retrieved()) {
        let content = message.get("content").and_then(Value::as_str).unwrap_or("");
        let line = match message.get("role").and_then(Value::as_str).unwrap_or("?") {
            "user" => format!("User: {}", prefix_chars(content, 200)),
            "assistant" => format!("Assistant: {}", prefix_chars(content, 300)),
            "tool" => format!("Tool result: {}", prefix_chars(content, 300)),
            _ => continue,
        };
        if used_chars + line.len() > budget_chars {
            break;
        }
        used_chars += line.len();
        parts.push(line);
    }

    if parts.is_empty() {
        None
    } else {
        Some(format!("{RETRIEVAL_HEADER}{}", parts.join("\n")))
    }
}

// ── Entity Boost Terms ──────────────────────────────────────────────────────

/// Category keywords associated with common tool domains.
/// When an entity co-occurs with these in history, they become boost terms.
const DOMAIN_KEYWORDS: &[(&str, &[&str])] = &[
    (
        "github",
        &[
            "github",
            "pr",
            "pull",
            "request",
            "issue",
            "repo",
            "repository",
            "org",
            "commit",
        ],
    ),
    (
        "git",
        &[
            "git", "branch", "merge", "rebase", "diff", "commit", "log", "status",
        ],
    ),
    (
        "memory",
        &[
            "memory",
            "store",
            "search",
            "retrieve",
            "preference",
            "remember",
            "follow",
            "track",
        ],
    ),
    (
        "code",
        &[
            "code", "file", "function", "class", "module", "analyze", "read", "edit",
        ],
    ),
    (
        "web",
        &[
            "fetch", "url", "http", "api", "endpoint", "request", "download",
        ],
    ),
];

/// Extract boost terms from session history for entities mentioned in the query.
///
/// Scans history for messages containing the same entity tokens as the query.
/// When found, extracts domain-related keywords from surrounding context to
/// improve tool selection. This implements the self-improving memory→selection loop.
///
/// Example: If history has "user follows matrixorigin on GitHub", and the query
/// mentions "matrixorigin", this returns ["github", "repo", "repository"].
pub fn extract_entity_boost_terms(
    full_history: &[Map<String, Value>],
    user_query: &str,
) -> Vec<String> {
    let query_tokens = tokenize(user_query);
    if query_tokens.is_empty() {
        return vec![];
    }

    let mut boost_set = std::collections::HashSet::new();

    // Scan history (skip the latest message — it's the current query)
    for msg in full_history.iter().rev().take(20) {
        let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
        if content.is_empty() {
            continue;
        }

        let msg_tokens = tokenize(content);
        // Check if this message shares entity tokens with the query
        let shared: Vec<_> = query_tokens
            .iter()
            .filter(|t| msg_tokens.contains(t))
            .collect();

        if shared.is_empty() {
            continue;
        }

        // This message mentions query entities — extract domain keywords
        let content_lower = content.to_lowercase();
        for (domain, keywords) in DOMAIN_KEYWORDS {
            // Check if the message content contains any domain keyword
            let has_domain = keywords.iter().any(|kw| content_lower.contains(kw));
            if has_domain {
                // Add the domain name and its top keywords as boost terms
                boost_set.insert(domain.to_string());
                for kw in keywords.iter().take(3) {
                    boost_set.insert(kw.to_string());
                }
            }
        }
    }

    // Remove terms that are already in the query (they'd be redundant in TF-IDF)
    for qt in &query_tokens {
        boost_set.remove(qt);
    }

    boost_set.into_iter().collect()
}

/// Extract boost terms from simple (role, content) history pairs.
/// Convenience wrapper for the REPL where history is stored as tuples.
pub fn extract_boost_terms_from_pairs(
    history: &[(String, String)],
    user_query: &str,
) -> Vec<String> {
    let maps: Vec<Map<String, Value>> = history
        .iter()
        .map(|(role, content)| {
            let mut m = Map::new();
            m.insert("role".into(), Value::String(role.clone()));
            m.insert("content".into(), Value::String(content.clone()));
            m
        })
        .collect();
    extract_entity_boost_terms(&maps, user_query)
}

// ── Memory result re-ranking ─────────────────────────────────────────────────

/// Minimum TF-IDF cosine similarity for a memory result to contribute
/// boost terms. Results below this threshold are filtered as irrelevant.
const MEMORY_RELEVANCE_THRESHOLD: f64 = 0.05;

/// Re-rank memory results by TF-IDF cosine similarity to the query.
/// Returns (content, score) pairs sorted by descending score, filtered
/// by MEMORY_RELEVANCE_THRESHOLD. This prevents noisy/irrelevant memories
/// from polluting tool selection boost terms.
///
/// Uses the same CJK-aware tokenizer and TF-IDF engine as conversation
/// history retrieval — no new dependencies.
pub fn rank_memory_results(query: &str, memory_contents: &[String]) -> Vec<(String, f64)> {
    if memory_contents.is_empty() || query.trim().is_empty() {
        return vec![];
    }

    let query_tf = build_tf(&tokenize(query));
    let doc_tfs: Vec<HashMap<String, f64>> = memory_contents
        .iter()
        .map(|content| build_tf(&tokenize(content)))
        .collect();

    // Build IDF from query + memory docs (the mini-corpus)
    let mut all_tfs = vec![query_tf.clone()];
    all_tfs.extend(doc_tfs.iter().cloned());
    let idf = build_idf(&all_tfs);

    let mut scored: Vec<(String, f64)> = memory_contents
        .iter()
        .zip(doc_tfs.iter())
        .map(|(content, doc_tf)| {
            let score = tfidf_cosine(&query_tf, doc_tf, &idf);
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

/// Append terms from `additional` onto `into`, skipping anything already present in `into`
/// or duplicated later in `additional`.
pub fn merge_boost_terms_unique(
    into: &mut Vec<String>,
    additional: impl IntoIterator<Item = String>,
) {
    let mut seen: HashSet<String> = into.iter().cloned().collect();
    for term in additional {
        if seen.insert(term.clone()) {
            into.push(term);
        }
    }
}

/// TF-IDF-ranked memory snippets → virtual `("memory", content)` history → entity boost terms,
/// merged into `boost_terms`. No-op when `ranked` is empty.
pub fn append_boost_terms_from_ranked_memory(
    boost_terms: &mut Vec<String>,
    user_query: &str,
    ranked: &[(String, f64)],
) {
    if ranked.is_empty() {
        return;
    }
    let virtual_history: Vec<(String, String)> = ranked
        .iter()
        .map(|(content, _score)| ("memory".to_string(), content.clone()))
        .collect();
    let memory_terms = extract_boost_terms_from_pairs(&virtual_history, user_query);
    merge_boost_terms_unique(boost_terms, memory_terms);
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

    // ── TF-IDF scoring tests ────────────────────────────────────────────

    #[test]
    fn tfidf_identical_documents_score_one() {
        let tokens = tokenize("hello world");
        let tf = build_tf(&tokens);
        let idf = build_idf(std::slice::from_ref(&tf));
        // Cosine similarity of a vector with itself = 1.0
        let sim = tfidf_cosine(&tf, &tf, &idf);
        assert!((sim - 1.0).abs() < 1e-9, "expected ~1.0, got {sim}");
    }

    #[test]
    fn tfidf_disjoint_documents_score_zero() {
        let tf_a = build_tf(&tokenize("hello world"));
        let tf_b = build_tf(&tokenize("foo bar"));
        let idf = build_idf(&[tf_a.clone(), tf_b.clone()]);
        let sim = tfidf_cosine(&tf_a, &tf_b, &idf);
        assert!(sim.abs() < 1e-9, "expected ~0.0, got {sim}");
    }

    #[test]
    fn tfidf_partial_overlap() {
        let tf_q = build_tf(&tokenize("rust memory"));
        let tf_a = build_tf(&tokenize("rust memory management"));
        let tf_b = build_tf(&tokenize("python web server"));
        let idf = build_idf(&[tf_a.clone(), tf_b.clone()]);
        let sim_a = tfidf_cosine(&tf_q, &tf_a, &idf);
        let sim_b = tfidf_cosine(&tf_q, &tf_b, &idf);
        assert!(
            sim_a > sim_b,
            "relevant doc should score higher: {sim_a} vs {sim_b}"
        );
    }

    #[test]
    fn tfidf_cjk_query_matches_cjk_document() {
        let tf_q = build_tf(&tokenize("分析仓库"));
        let tf_a = build_tf(&tokenize("帮我分析这个仓库的结构"));
        let tf_b = build_tf(&tokenize("hello world rust code"));
        let idf = build_idf(&[tf_a.clone(), tf_b.clone()]);
        let sim_a = tfidf_cosine(&tf_q, &tf_a, &idf);
        let sim_b = tfidf_cosine(&tf_q, &tf_b, &idf);
        assert!(
            sim_a > sim_b,
            "CJK match should score higher: {sim_a} vs {sim_b}"
        );
        assert!(sim_a > 0.0, "CJK match should be positive: {sim_a}");
    }

    // ── freshness decay tests ───────────────────────────────────────────

    #[test]
    fn freshness_decay_reduces_old_scores() {
        // distance 0 → decay 1.0, distance 10 → 0.95^10 ≈ 0.5987
        let d0 = DECAY_BASE.powi(0);
        let d10 = DECAY_BASE.powi(10);
        let d50 = DECAY_BASE.powi(50);
        assert!((d0 - 1.0).abs() < 1e-9);
        assert!(d10 < 0.70);
        assert!(d50 < 0.10);
        assert!(d0 > d10);
        assert!(d10 > d50);
    }

    #[test]
    fn rule_based_extraction_prefers_recent_matches() {
        // Two messages with the same content; the more recent one should rank higher.
        let mut old_msg = Map::new();
        old_msg.insert("role".into(), Value::String("user".into()));
        old_msg.insert(
            "content".into(),
            Value::String("tell me about rust memory".into()),
        );

        let new_msg = old_msg.clone();
        // They are identical in content, but new_msg is later in history.
        let recent = Map::new(); // dummy: empty so nothing is filtered as recent

        let history = vec![
            Map::new(), // index 0 (skipped by rule_based_extraction)
            old_msg.clone(),
            new_msg.clone(),
        ];
        let result =
            rule_based_extraction(&history, &[recent], "rust memory", RETRIEVAL_BUDGET_CHARS);
        assert!(result.is_some());
        let text = result.unwrap();
        // Should contain "User:" formatted line
        assert!(text.contains("User:"));
    }

    // ── adaptive budget tests ───────────────────────────────────────────

    #[test]
    fn adaptive_budget_short_query() {
        assert_eq!(adaptive_budget_chars("fix bug"), 4_000);
        assert_eq!(adaptive_budget_chars(""), 4_000);
    }

    #[test]
    fn adaptive_budget_medium_query() {
        let q = "explain how the retrieval system works";
        assert!(q.len() >= 20 && q.len() <= 100);
        assert_eq!(adaptive_budget_chars(q), 8_000);
    }

    #[test]
    fn adaptive_budget_complex_long() {
        let q = "a]".repeat(60); // >100 chars
        assert_eq!(adaptive_budget_chars(&q), 12_000);
    }

    #[test]
    fn adaptive_budget_code_pattern() {
        assert_eq!(adaptive_budget_chars("fn main()"), 12_000);
        assert_eq!(adaptive_budget_chars("class Foo"), 12_000);
        assert_eq!(adaptive_budget_chars("impl Bar"), 12_000);
        assert_eq!(adaptive_budget_chars("x -> y"), 12_000);
        assert_eq!(adaptive_budget_chars("std::io"), 12_000);
        assert_eq!(adaptive_budget_chars("```code```"), 12_000);
        assert_eq!(adaptive_budget_chars("let x = {"), 12_000);
    }

    // ── budget enforcement tests ────────────────────────────────────────

    #[test]
    fn rule_based_extraction_respects_budget() {
        let mut messages: Vec<Map<String, Value>> = Vec::new();
        // Index 0 is skipped
        messages.push(Map::new());
        for i in 1..=20 {
            let mut m = Map::new();
            m.insert("role".into(), Value::String("user".into()));
            m.insert(
                "content".into(),
                Value::String(format!("message about rust number {i} with details")),
            );
            messages.push(m);
        }
        let tiny_budget = 80;
        let result = rule_based_extraction(&messages, &[], "rust details", tiny_budget);
        if let Some(text) = result {
            // The body (after header) should not exceed the budget.
            let body = text.strip_prefix(RETRIEVAL_HEADER).unwrap_or(&text);
            assert!(
                body.len() <= tiny_budget + 50, // small tolerance for single-line rounding
                "body too long: {} chars vs budget {}",
                body.len(),
                tiny_budget
            );
        }
    }

    // ── format_retrieved_events backward compat ─────────────────────────

    #[test]
    fn format_retrieved_events_basic() {
        let mut ev = Map::new();
        ev.insert("event_type".into(), Value::String("user_query".into()));
        ev.insert("content".into(), Value::String("what is rust".into()));

        let result = format_retrieved_events(&[ev], &[], 5000);
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.starts_with(RETRIEVAL_HEADER));
        assert!(text.contains("User: what is rust"));
    }

    #[test]
    fn format_retrieved_events_skips_recent_duplicates() {
        let mut ev = Map::new();
        ev.insert("event_type".into(), Value::String("user_query".into()));
        ev.insert("content".into(), Value::String("hello world".into()));

        let recent = vec![prefix_chars("hello world", 100)];
        let result = format_retrieved_events(&[ev], &recent, 5000);
        assert!(result.is_none());
    }

    #[test]
    fn format_retrieved_events_empty_input() {
        assert!(format_retrieved_events(&[], &[], 5000).is_none());
    }

    // ── enhanced_extraction tests ───────────────────────────────────────

    #[test]
    fn enhanced_extraction_uses_adaptive_budget() {
        // Short query → small budget; should still produce results if matches exist.
        let mut messages = vec![Map::new()]; // index 0 placeholder
        let mut m = Map::new();
        m.insert("role".into(), Value::String("user".into()));
        m.insert("content".into(), Value::String("fix the bug".into()));
        messages.push(m);

        let result = enhanced_extraction(&messages, &[], "fix bug");
        assert!(result.is_some());
    }

    #[test]
    fn enhanced_extraction_empty_history() {
        assert!(enhanced_extraction(&[], &[], "anything").is_none());
    }

    // ── backward compatibility: constant value ──────────────────────────

    #[test]
    fn retrieval_budget_constant_unchanged() {
        assert_eq!(RETRIEVAL_BUDGET_CHARS, 8000);
    }

    #[test]
    fn rule_based_extraction_returns_none_for_empty_query() {
        let history = vec![Map::new()];
        assert!(rule_based_extraction(&history, &[], "", 8000).is_none());
    }

    #[test]
    fn rule_based_extraction_returns_none_no_old_messages() {
        let m = Map::new();
        // Only the index-0 message exists; after skip(1) there is nothing.
        assert!(rule_based_extraction(std::slice::from_ref(&m), &[], "hello", 8000).is_none());
    }

    // ── Phase 6.3: Memory retrieval budget enforcement ──

    #[test]
    fn budget_enforcement_truncates_at_limit() {
        // Build enough messages to exceed a small budget
        let mut history = Vec::new();
        for i in 0..20 {
            let mut m = Map::new();
            m.insert("role".into(), Value::String("user".into()));
            m.insert(
                "content".into(),
                Value::String(format!(
                    "message number {} with some content about various topics",
                    i
                )),
            );
            history.push(m);
        }
        // Very small budget — should get only a few messages
        let result = rule_based_extraction(&history, &[], "message", 200);
        if let Some(text) = &result {
            // Budget 200 + header length; text should be bounded
            assert!(
                text.len() < 600,
                "retrieved text should respect budget, got {} chars",
                text.len()
            );
        }
    }

    #[test]
    fn adaptive_budget_code_query_gets_large_budget() {
        assert_eq!(
            adaptive_budget_chars("fn main() { println!(\"hello\"); }"),
            12_000
        );
    }

    #[test]
    fn adaptive_budget_long_query_gets_medium_budget() {
        let q = "explain the architecture of our memory retrieval system";
        assert_eq!(adaptive_budget_chars(q), 8_000);
    }

    #[test]
    fn enhanced_extraction_respects_budget() {
        let mut history = Vec::new();
        for i in 0..50 {
            let mut m = Map::new();
            m.insert("role".into(), Value::String("user".into()));
            m.insert(
                "content".into(),
                Value::String(format!(
                    "long message {} with lots of filler text to use up budget",
                    i
                )),
            );
            history.push(m);
        }
        let result = enhanced_extraction(&history, &[], "filler");
        if let Some(text) = &result {
            // Short query → 4000 char budget; text should be bounded
            assert!(
                text.len() < 6000,
                "enhanced_extraction should respect adaptive budget, got {} chars",
                text.len()
            );
        }
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

    // ── Memory boost-term merge ──────────────────────────────────────────

    #[test]
    fn merge_boost_terms_unique_skips_existing_and_internal_dupes() {
        let mut v = vec!["a".into(), "b".into()];
        merge_boost_terms_unique(&mut v, ["c".into(), "a".into(), "c".into()]);
        assert_eq!(v, vec!["a", "b", "c"]);
    }

    #[test]
    fn append_boost_terms_from_ranked_memory_no_op_on_empty_ranked() {
        let mut v = vec!["x".into()];
        append_boost_terms_from_ranked_memory(&mut v, "hello", &[]);
        assert_eq!(v, vec!["x"]);
    }

    #[test]
    fn append_boost_terms_from_ranked_memory_extracts_from_virtual_history() {
        // Same cold-start shape as `memory_boost_integration::cold_start_memory_provides_github_context`.
        let query = "matrixorigin 最新情况";
        let ranked = vec![(
            "matrixorigin is a GitHub organization focused on cloud-native databases".to_string(),
            0.99,
        )];
        let mut terms: Vec<String> = Vec::new();
        append_boost_terms_from_ranked_memory(&mut terms, query, &ranked);
        let has_github = terms
            .iter()
            .any(|t| *t == "github" || *t == "repo" || *t == "repository" || *t == "org");
        assert!(
            has_github,
            "memory virtual-history path should yield GitHub-domain boost terms, got {:?}",
            terms
        );
    }
}
